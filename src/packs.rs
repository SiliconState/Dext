use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::session::dext_state_dir;
use crate::{byte_prefix_at_char_boundary, cap_bytes_with_hint};

const PACK_PROMPT_CAP: usize = 32_000;
const PACK_LIST_LIMIT: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackInfo {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) path: PathBuf,
    pub(crate) pack_md_path: PathBuf,
    pub(crate) phooks_path: Option<PathBuf>,
    pub(crate) source: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PackInvocation {
    pub(crate) pack: PackInfo,
    pub(crate) task: String,
}

#[derive(Default)]
struct PackFrontMatter {
    name: Option<String>,
    description: Option<String>,
}

impl PackInfo {
    pub(crate) fn env_var_name(&self) -> String {
        pack_env_var_name(&self.name)
    }
}

pub(crate) fn pack_env_var_name(name: &str) -> String {
    let mut out = String::from("DEXT_PACK_");
    let mut last_sep = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
            last_sep = false;
        } else if !last_sep {
            out.push('_');
            last_sep = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out.push_str("_DIR");
    out
}

fn normalize_key(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

fn trim_yaml_scalar(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        if (bytes[0] == b'\'' && bytes[trimmed.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[trimmed.len() - 1] == b'"')
        {
            return trimmed[1..trimmed.len() - 1].trim().to_string();
        }
    }
    trimmed.to_string()
}

fn parse_front_matter(text: &str) -> PackFrontMatter {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return PackFrontMatter::default();
    }
    let mut front = PackFrontMatter::default();
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" {
            break;
        }
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        match key.trim() {
            "name" => front.name = Some(trim_yaml_scalar(value)),
            "description" => front.description = Some(trim_yaml_scalar(value)),
            _ => {}
        }
    }
    front
}

fn load_pack_from_dir(dir: &Path, source: &str) -> Result<Option<PackInfo>> {
    let pack_md_path = dir.join("PACK.md");
    if !pack_md_path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&pack_md_path)
        .with_context(|| format!("reading {}", pack_md_path.display()))?;
    let front = parse_front_matter(&text);
    let name = front.name.unwrap_or_else(|| {
        dir.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("pack")
            .to_string()
    });
    let description = front.description.unwrap_or_default();
    let path = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let pack_md_path = std::fs::canonicalize(&pack_md_path).unwrap_or(pack_md_path);
    let phooks = path.join("phooks.json");
    Ok(Some(PackInfo {
        name,
        description,
        path,
        pack_md_path,
        phooks_path: phooks.is_file().then_some(phooks),
        source: source.to_string(),
    }))
}

fn push_root(roots: &mut Vec<(PathBuf, String)>, path: PathBuf, label: impl Into<String>) {
    if path.is_dir() {
        roots.push((path, label.into()));
    }
}

fn push_direct(dirs: &mut Vec<(PathBuf, String)>, path: PathBuf, label: impl Into<String>) {
    if path.join("PACK.md").is_file() {
        dirs.push((path, label.into()));
    }
}

fn candidate_pack_dirs(root: &Path) -> Vec<(PathBuf, String)> {
    let mut direct = Vec::new();
    for (key, value) in std::env::vars() {
        if key.starts_with("DEXT_PACK_") && key.ends_with("_DIR") && !value.trim().is_empty() {
            push_direct(&mut direct, PathBuf::from(value), format!("env:{key}"));
        }
    }

    let mut roots = Vec::new();
    push_root(&mut roots, root.join(".dext/packs"), "project:.dext/packs");
    push_root(&mut roots, root.join("packs"), "project:packs");
    if let Some(paths) = std::env::var_os("DEXT_PACKS_DIR") {
        for path in std::env::split_paths(&paths) {
            push_root(&mut roots, path, "env:DEXT_PACKS_DIR");
        }
    }
    push_root(
        &mut roots,
        dext_state_dir().join("packs"),
        "user:~/.dext/packs",
    );
    push_root(
        &mut roots,
        Path::new(env!("CARGO_MANIFEST_DIR")).join("packs"),
        "bundled:packs",
    );

    for (pack_root, label) in roots {
        push_direct(&mut direct, pack_root.clone(), label.clone());
        let Ok(entries) = std::fs::read_dir(&pack_root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join("PACK.md").is_file() {
                direct.push((path, label.clone()));
            }
        }
    }
    direct
}

pub(crate) fn discover_packs(root: &Path) -> Vec<PackInfo> {
    let mut packs = Vec::new();
    let mut seen_names = HashSet::new();
    let mut seen_paths = HashSet::new();
    for (dir, source) in candidate_pack_dirs(root) {
        let path_key = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if !seen_paths.insert(path_key) {
            continue;
        }
        let Ok(Some(pack)) = load_pack_from_dir(&dir, &source) else {
            continue;
        };
        let key = normalize_key(&pack.name);
        if seen_names.insert(key) {
            packs.push(pack);
        }
    }
    packs.sort_by(|a, b| a.name.cmp(&b.name));
    packs
}

pub(crate) fn find_pack(root: &Path, selector: &str) -> Result<PackInfo> {
    let selector = selector.trim();
    if selector.is_empty() {
        anyhow::bail!("missing pack name");
    }
    let key = normalize_key(selector);
    let packs = discover_packs(root);
    if let Some(pack) = packs.iter().find(|pack| normalize_key(&pack.name) == key) {
        return Ok(pack.clone());
    }
    let matches: Vec<PackInfo> = packs
        .into_iter()
        .filter(|pack| normalize_key(&pack.name).starts_with(&key))
        .collect();
    match matches.as_slice() {
        [pack] => Ok(pack.clone()),
        [] => anyhow::bail!("pack '{selector}' not found. Run /pack list."),
        many => anyhow::bail!(
            "pack '{selector}' is ambiguous: {}",
            many.iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub(crate) fn render_pack_listing(root: &Path) -> String {
    let packs = discover_packs(root);
    if packs.is_empty() {
        return "packs: none found\nsearch paths: .dext/packs, packs, DEXT_PACKS_DIR, ~/.dext/packs, bundled packs".to_string();
    }
    let mut out = String::from("packs:\n");
    for pack in packs.iter().take(PACK_LIST_LIMIT) {
        let desc = if pack.description.is_empty() {
            "(no description)".to_string()
        } else {
            pack.description.clone()
        };
        out.push_str(&format!(
            "  {} — {}\n    path: {}\n",
            pack.name,
            desc,
            pack.path.display()
        ));
    }
    if packs.len() > PACK_LIST_LIMIT {
        out.push_str(&format!(
            "  … [{} more packs omitted]\n",
            packs.len() - PACK_LIST_LIMIT
        ));
    }
    out.push_str(
        "commands: /pack run <name> <task> · /pack inspect <name> · dext pack run <name> <task>",
    );
    out
}

pub(crate) fn render_pack_inspect(root: &Path, selector: &str) -> Result<String> {
    let pack = find_pack(root, selector)?;
    let hooks = pack
        .phooks_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(none)".to_string());
    Ok(format!(
        "pack: {}\ndescription: {}\nsource: {}\npath: {}\nworkflow: {}\nhooks: {}\nenv: {}={}",
        pack.name,
        if pack.description.is_empty() {
            "(none)"
        } else {
            &pack.description
        },
        pack.source,
        pack.path.display(),
        pack.pack_md_path.display(),
        hooks,
        pack.env_var_name(),
        pack.path.display()
    ))
}

pub(crate) fn pack_prompt(pack: &PackInfo, task: &str) -> Result<String> {
    let workflow = std::fs::read_to_string(&pack.pack_md_path)
        .with_context(|| format!("reading {}", pack.pack_md_path.display()))?;
    let workflow = cap_bytes_with_hint(
        workflow,
        PACK_PROMPT_CAP,
        "PACK.md truncated; inspect the pack source if missing details matter.",
    );
    let hook_line = pack
        .phooks_path
        .as_ref()
        .map(|p| format!("Pack hook template: {}", p.display()))
        .unwrap_or_else(|| "Pack hook template: none".to_string());
    let task = task.trim();
    let task = if task.is_empty() {
        "Run this pack. If required inputs are missing, ask once for the minimum needed details."
            .to_string()
    } else {
        task.to_string()
    };
    Ok(format!(
        "[dext pack invocation]\nPack: {name}\nDescription: {description}\nPack path: {path}\nWorkflow: {workflow_path}\n{hook_line}\nPack env: {env_name}={path}\n\nFollow the PACK.md workflow below. Treat this as an explicit user request to invoke the pack; do not just describe how to run it. Use normal Dext tools and pack-local helper scripts through bash when the workflow says to.\n\n--- PACK.md ---\n{workflow}\n--- END PACK.md ---\n\nUser task for this pack:\n{task}",
        name = pack.name,
        description = if pack.description.is_empty() {
            "(none)"
        } else {
            &pack.description
        },
        path = pack.path.display(),
        workflow_path = pack.pack_md_path.display(),
        env_name = pack.env_var_name(),
    ))
}

fn normalized_words(raw: &str) -> String {
    raw.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn has_invocation_pattern(text: &str, name: &str) -> bool {
    if !text.contains(name) {
        return false;
    }
    let starts = [
        format!("run {name}"),
        format!("use {name}"),
        format!("start {name}"),
        format!("launch {name}"),
        format!("invoke {name}"),
        format!("execute {name}"),
        format!("apply {name}"),
    ];
    if starts.iter().any(|s| text.starts_with(s)) {
        return true;
    }
    let patterns = [
        format!("run {name} "),
        format!("use {name} "),
        format!("start {name} "),
        format!("launch {name} "),
        format!("invoke {name} "),
        format!("execute {name} "),
        format!("apply {name} "),
        format!("run the {name} pack"),
        format!("use the {name} pack"),
        format!("start the {name} pack"),
        format!("launch the {name} pack"),
        format!("invoke the {name} pack"),
        format!("execute the {name} pack"),
        format!("run pack {name}"),
        format!("use pack {name}"),
        format!("run the pack {name}"),
        format!("use the pack {name}"),
    ];
    patterns.iter().any(|p| text.contains(p))
}

pub(crate) fn infer_pack_invocation(root: &Path, user_input: &str) -> Option<PackInvocation> {
    let text = normalized_words(user_input);
    if text.contains("what is ") || text.contains("explain ") {
        return None;
    }
    let mut packs = discover_packs(root);
    packs.sort_by_key(|pack| std::cmp::Reverse(pack.name.len()));
    for pack in packs {
        let name = normalize_key(&pack.name);
        if has_invocation_pattern(&text, &name) {
            return Some(PackInvocation {
                pack,
                task: user_input.trim().to_string(),
            });
        }
    }
    None
}

pub(crate) fn pack_summary_for_prompt(root: &Path) -> Option<String> {
    let packs = discover_packs(root);
    if packs.is_empty() {
        return None;
    }
    let mut _counts: HashMap<&str, usize> = HashMap::new();
    for pack in &packs {
        *_counts.entry(pack.source.as_str()).or_insert(0) += 1;
    }
    let mut out = String::from("Available Dext packs: ");
    out.push_str(
        &packs
            .iter()
            .take(10)
            .map(|pack| pack.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    );
    if packs.len() > 10 {
        out.push_str(&format!(", … +{}", packs.len() - 10));
    }
    out.push_str(". Invoke with `/pack run <name> <task>`, `dext pack run <name> <task>`, or conversationally (for example, 'run autoresearch on …').");
    Some(byte_prefix_at_char_boundary(&out, 1_000).to_string())
}
