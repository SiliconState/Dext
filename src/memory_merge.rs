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
    let mut current_heading_path: Vec<String> = Vec::new();
    let mut current_heading_line = String::new();
    let mut current_body = String::new();
    let mut in_preamble = true;

    for line in input.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('#') {
            let hashes = trimmed.len() - rest.len();
            let level = hashes.min(6);
            if level >= 1 {
                if in_preamble {
                    // First heading ends preamble
                    in_preamble = false;
                } else if !current_heading_line.is_empty() {
                    // Flush previous section
                    sections.push(Section {
                        heading_path: current_heading_path.clone(),
                        heading_line: current_heading_line.clone(),
                        body: current_body.trim_end().to_string(),
                    });
                    current_body.clear();
                }
                current_heading_line = trimmed.to_string();
                current_heading_path = trimmed
                    .trim_start_matches('#')
                    .trim()
                    .split(' ')
                    .map(String::from)
                    .collect();
                continue;
            }
        }
        if in_preamble {
            if !preamble.is_empty() || !line.is_empty() {
                preamble.push_str(line);
                preamble.push('\n');
            }
        } else {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }

    // Flush last section
    if !current_heading_line.is_empty() {
        sections.push(Section {
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

fn section_key(s: &Section) -> String {
    s.heading_path.join(" ").to_ascii_lowercase()
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
    for s in base_parsed.sections.iter()
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
            (Some(b), Some(o), Some(t))
                if o.body == b.body && t.body == b.body =>
            {
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
            // Both changed differently — conflict
            (Some(_), Some(o), Some(t)) => {
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
            // New in both — union
            (None, Some(o), Some(t)) => {
                if o.body == t.body {
                    sections.push((*o).clone());
                } else {
                    sections.push((*o).clone());
                    sections.push((*t).clone());
                    warnings.push(format!(
                        "new section '{}' in both sides; kept both",
                        key
                    ));
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
    let warnings = Vec::new();

    // Split into lines, dedupe by normalized content
    let base_lines: Vec<&str> = base.lines().collect();
    let ours_lines: Vec<&str> = ours.lines().collect();
    let theirs_lines: Vec<&str> = theirs.lines().collect();

    let mut seen = std::collections::HashSet::new();
    let mut result: Vec<String> = Vec::new();

    // Start with base
    for line in &base_lines {
        let norm = normalize_recall_line(line);
        if !norm.is_empty() {
            seen.insert(norm);
        }
        result.push(line.to_string());
    }

    // Add ours additions
    for line in &ours_lines {
        let norm = normalize_recall_line(line);
        if norm.is_empty() || seen.contains(&norm) {
            continue;
        }
        seen.insert(norm);
        result.push(line.to_string());
    }

    // Add theirs additions
    for line in &theirs_lines {
        let norm = normalize_recall_line(line);
        if norm.is_empty() || seen.contains(&norm) {
            continue;
        }
        seen.insert(norm);
        result.push(line.to_string());
    }

    // Remove lines that were in base but missing from both ours and theirs
    let ours_set: std::collections::HashSet<String> =
        ours_lines.iter().map(|l| normalize_recall_line(l)).collect();
    let theirs_set: std::collections::HashSet<String> =
        theirs_lines.iter().map(|l| normalize_recall_line(l)).collect();
    result.retain(|line| {
        let norm = normalize_recall_line(line);
        if norm.is_empty() {
            return true;
        }
        // Keep if it wasn't in base, or if at least one side still has it
        !base_lines.iter().any(|l| normalize_recall_line(l) == norm)
            || ours_set.contains(&norm)
            || theirs_set.contains(&norm)
    });

    let content = result.join("\n");
    if content.ends_with('\n') {
        // fine
    } else if !content.is_empty() {
        // add trailing newline
    }

    MergeOutcome {
        content,
        clean: warnings.is_empty(),
        warnings,
    }
}

fn normalize_recall_line(line: &str) -> String {
    line.trim()
        .trim_start_matches("- ")
        .trim_start_matches("- ")
        .trim()
        .to_ascii_lowercase()
}

pub(crate) fn register(
    repo: &Path,
    mode: RegisterMode,
    versioned_attributes: bool,
) -> Result<(), String> {
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
    run_git(
        repo,
        &[
            "config",
            &format!("merge.{driver_name}.driver"),
            &format!("dext memory merge {} %O %A %B %L %P", if mode == RegisterMode::Recall { "--recall" } else { "" }),
        ],
    )?;

    // Write gitattributes
    let attr_path = if versioned_attributes {
        repo.join(".gitattributes")
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
            .filter(|l| {
                !l.contains("merge=dext-memory") && !l.contains("merge=dext-recall")
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&attr_path, format!("{filtered}\n"))
            .map_err(|e| format!("write attributes: {e}"))?;
    }

    Ok(())
}

pub(crate) fn check(repo: &Path) -> Result<MemoryMergeStatus, String> {
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
        &["config", "--local", &format!("merge.{MERGE_DRIVER_MEMORY}.driver")],
    )
    .is_ok();
    let recall_config = run_git(
        repo,
        &["config", "--local", &format!("merge.{MERGE_DRIVER_RECALL}.driver")],
    )
    .is_ok();

    let local_attr = git_dir.join("info").join("attributes");
    let versioned_attr = repo.join(".gitattributes");

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

fn find_git_dir(repo: &Path) -> Result<PathBuf, String> {
    let out = run_git(repo, &["rev-parse", "--git-dir"])?;
    let git_dir = repo.join(out.trim());
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

