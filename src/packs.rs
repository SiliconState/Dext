use anyhow::{Context, Result};
use serde::{Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::list_render;
use crate::session::{canonicalize_or_clone, dext_state_dir};
use crate::{byte_prefix_at_char_boundary, cap_bytes_with_hint};

const PACK_PROMPT_CAP: usize = 32_000;
const PACK_FILE_CAP: u64 = 1024 * 1024;
const PACK_FORK_FILE_CAP: u64 = 16 * 1024 * 1024;
const PACK_FORK_TOTAL_CAP: u64 = 256 * 1024 * 1024;
const PACK_FORK_ENTRY_CAP: usize = 4_096;
const PACK_FORK_DEPTH_CAP: usize = 32;
const PACK_LIST_LIMIT: usize = 50;

type PackDirCandidate = (PathBuf, String, Option<String>);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct PackUi {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) starter_prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) artifact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) time_to_first_artifact: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) requires: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) gallery: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct PackInfo {
    pub(crate) name: String,
    pub(crate) description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) shelf: Option<String>,
    pub(crate) source: String,
    #[serde(serialize_with = "serialize_absolute_path")]
    pub(crate) path: PathBuf,
    #[serde(rename = "pack_md", serialize_with = "serialize_absolute_path")]
    pub(crate) pack_md_path: PathBuf,
    #[serde(skip)]
    pub(crate) phooks_path: Option<PathBuf>,
    #[serde(
        rename = "runtime",
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_absolute_path"
    )]
    pub(crate) runtime_path: Option<PathBuf>,
    pub(crate) credential_env: Vec<String>,
    #[serde(skip)]
    pub(crate) credential_env_ignored: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) ui: Option<PackUi>,
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
    credential_env: Vec<String>,
    ui: Option<PackUi>,
}

#[derive(Default)]
struct PackCopyBudget {
    entries: usize,
    bytes: u64,
}

impl PackCopyBudget {
    fn add_entry(&mut self, path: &Path) -> Result<()> {
        self.entries = self.entries.saturating_add(1);
        if self.entries > PACK_FORK_ENTRY_CAP {
            anyhow::bail!(
                "source pack exceeds the {} entry fork limit near {}",
                PACK_FORK_ENTRY_CAP,
                path.display()
            );
        }
        Ok(())
    }

    fn add_bytes(&mut self, path: &Path, bytes: u64) -> Result<()> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes > PACK_FORK_TOTAL_CAP {
            anyhow::bail!(
                "source pack exceeds the {} byte total fork limit near {}",
                PACK_FORK_TOTAL_CAP,
                path.display()
            );
        }
        Ok(())
    }
}

fn serialize_absolute_path<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&canonicalize_or_clone(path).to_string_lossy())
}

fn serialize_optional_absolute_path<S>(
    path: &Option<PathBuf>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    path.as_ref()
        .map(|path| canonicalize_or_clone(path))
        .as_ref()
        .map(|path| path.to_string_lossy())
        .serialize(serializer)
}

impl PackInfo {
    pub(crate) fn env_var_name(&self) -> String {
        pack_env_var_name(&self.name)
    }

    pub(crate) fn source_identity(&self) -> String {
        pack_source_identity(&self.source, &self.path)
    }

    #[cfg(test)]
    pub(crate) fn is_project(&self) -> bool {
        self.source.starts_with("project:")
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

fn parse_env_names(raw: &str) -> Vec<String> {
    let raw = trim_yaml_scalar(raw);
    let raw = raw
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(&raw);
    let mut names = raw
        .split(',')
        .map(trim_yaml_scalar)
        .filter(|name| {
            let mut chars = name.chars();
            chars
                .next()
                .is_some_and(|ch| ch.is_ascii_uppercase() || ch == '_')
                && chars.all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
        })
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names.truncate(32);
    names
}

fn normalized_ui_scalar(raw: &str, cap: usize) -> String {
    let value = list_render::terminal_safe_text(&trim_yaml_scalar(raw)).replace(['\n', '\t'], " ");
    capped_chars(value.trim(), cap)
}

fn parse_ui_list(raw: &str) -> Vec<String> {
    let raw = trim_yaml_scalar(raw);
    let raw = raw
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(&raw);
    raw.split(',')
        .map(|value| normalized_ui_scalar(value, usize::MAX))
        .filter(|value| !value.is_empty())
        .take(16)
        .collect()
}

fn capped_chars(raw: &str, cap: usize) -> String {
    raw.chars().take(cap).collect()
}

fn set_pack_ui(front: &mut PackFrontMatter, update: impl FnOnce(&mut PackUi)) {
    update(front.ui.get_or_insert_with(PackUi::default));
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
        let raw_key = key.trim();
        match raw_key {
            "name" => front.name = Some(trim_yaml_scalar(value)),
            "description" => front.description = Some(trim_yaml_scalar(value)),
            "credential-env" | "credential_env" => {
                front.credential_env.extend(parse_env_names(value));
                front.credential_env.sort();
                front.credential_env.dedup();
                front.credential_env.truncate(32);
            }
            _ => parse_ui_front_matter(&mut front, raw_key, value),
        }
    }
    front
}

fn parse_ui_front_matter(front: &mut PackFrontMatter, raw_key: &str, value: &str) {
    let key = raw_key.to_ascii_lowercase().replace('_', "-");
    match key.as_str() {
        "ui-starter-prompt" => {
            let value = normalized_ui_scalar(value, 400);
            set_pack_ui(front, |ui| ui.starter_prompt = Some(value));
        }
        "ui-artifact" => {
            let value = trim_yaml_scalar(value).to_ascii_lowercase();
            if matches!(
                value.as_str(),
                "html" | "chart" | "table" | "markdown" | "file" | "none"
            ) {
                set_pack_ui(front, |ui| ui.artifact = Some(value));
            }
        }
        "ui-time-to-first-artifact" => {
            let seconds = trim_yaml_scalar(value).parse::<f64>().ok();
            if let Some(seconds) = seconds.filter(|value| {
                value.is_finite() && *value >= 0.0 && value.round() < u64::MAX as f64
            }) {
                set_pack_ui(front, |ui| {
                    ui.time_to_first_artifact = Some(seconds.round() as u64);
                });
            }
        }
        "ui-requires" => {
            let values = parse_ui_list(value);
            set_pack_ui(front, |ui| ui.requires = Some(values));
        }
        "ui-gallery" => {
            let value = trim_yaml_scalar(value).to_ascii_lowercase();
            let enabled = match value.as_str() {
                "true" | "yes" | "1" => Some(true),
                "false" | "no" | "0" => Some(false),
                _ => None,
            };
            if let Some(enabled) = enabled {
                set_pack_ui(front, |ui| ui.gallery = Some(enabled));
            }
        }
        "ui-tags" => {
            let values = parse_ui_list(value);
            set_pack_ui(front, |ui| ui.tags = Some(values));
        }
        "ui-icon" => {
            let value = normalized_ui_scalar(value, 32);
            set_pack_ui(front, |ui| ui.icon = Some(value));
        }
        _ => {}
    }
}

fn read_pack_file(path: &Path) -> Result<String> {
    crate::read_utf8_regular_file_with_limit(path, PACK_FILE_CAP as usize, None, "pack workflow")
        .map_err(anyhow::Error::msg)
}

fn load_pack_from_dir(dir: &Path, source: &str, shelf: Option<&str>) -> Result<Option<PackInfo>> {
    let pack_md_path = dir.join("PACK.md");
    if !pack_md_path.is_file() {
        return Ok(None);
    }
    let text = read_pack_file(&pack_md_path)?;
    let front = parse_front_matter(&text);
    let name = front.name.unwrap_or_else(|| {
        dir.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("pack")
            .to_string()
    });
    let description = front.description.unwrap_or_default();
    let credential_env_ignored = source.starts_with("project:") && !front.credential_env.is_empty();
    let credential_env = if credential_env_ignored {
        Vec::new()
    } else {
        front.credential_env
    };
    let path = dir.to_path_buf();
    let phooks = path.join("phooks.json");
    let runtime = path.join(crate::pack_runtime::RUNTIME_MANIFEST_NAME);
    Ok(Some(PackInfo {
        name,
        description,
        path,
        pack_md_path,
        phooks_path: phooks.is_file().then_some(phooks),
        runtime_path: runtime.is_file().then_some(runtime),
        credential_env,
        credential_env_ignored,
        ui: front.ui,
        source: source.to_string(),
        shelf: shelf.map(str::to_string),
    }))
}

fn push_direct(
    dirs: &mut Vec<PackDirCandidate>,
    path: PathBuf,
    label: impl Into<String>,
    shelf: Option<String>,
) {
    // load_pack_from_dir already checks PACK.md existence
    dirs.push((path, label.into(), shelf));
}

fn push_shelf(dirs: &mut Vec<PackDirCandidate>, shelf_path: PathBuf, label: &str) {
    let shelf_name = shelf_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("shelf")
        .to_string();
    let packs_root = shelf_path.join("packs");
    let Ok(packs) = std::fs::read_dir(&packs_root) else {
        return;
    };
    let mut packs = packs.flatten().collect::<Vec<_>>();
    packs.sort_by_key(|entry| entry.path());
    for pack in packs {
        let is_dir = pack.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let path = pack.path();
        if path.join("PACK.md").is_file() {
            push_direct(
                dirs,
                path,
                format!("{label}/{shelf_name}"),
                Some(shelf_name.clone()),
            );
        }
    }
}

fn push_shelf_root(
    dirs: &mut Vec<PackDirCandidate>,
    shelf_root: PathBuf,
    label: impl Into<String>,
) {
    if !shelf_root.is_dir() {
        return;
    }
    let label = label.into();
    push_shelf(dirs, shelf_root.clone(), &label);
    let Ok(shelves) = std::fs::read_dir(&shelf_root) else {
        return;
    };
    let mut shelves = shelves.flatten().collect::<Vec<_>>();
    shelves.sort_by_key(|entry| entry.path());
    for shelf in shelves {
        let is_dir = shelf.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir {
            push_shelf(dirs, shelf.path(), &label);
        }
    }
}

fn shelf_root_fingerprint(path: &Path) -> String {
    let canonical = canonicalize_or_clone(path);
    let identity = if cfg!(windows) {
        canonical.to_string_lossy().to_lowercase()
    } else {
        canonical.to_string_lossy().to_string()
    };
    Sha256::digest(identity.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn pack_source_identity(source: &str, path: &Path) -> String {
    format!("{source}#{}", shelf_root_fingerprint(path))
}

fn candidate_pack_dirs(root: &Path) -> Vec<PackDirCandidate> {
    let mut direct = Vec::new();
    push_shelf_root(
        &mut direct,
        root.join(".dext/shelves"),
        "project:.dext/shelves",
    );

    if let Some(paths) = std::env::var_os("DEXT_SHELVES_DIR") {
        for path in std::env::split_paths(&paths) {
            push_shelf_root(&mut direct, path, "env:DEXT_SHELVES_DIR");
        }
    }

    push_shelf_root(
        &mut direct,
        dext_state_dir().join("shelves"),
        "user:~/.dext/shelves",
    );

    direct
}

fn discover_packs_with_project(root: &Path, include_project: bool) -> Vec<PackInfo> {
    let mut packs = Vec::new();
    let mut seen_names = HashSet::new();
    let mut seen_paths = HashSet::new();
    for (dir, source, shelf) in candidate_pack_dirs(root) {
        if !include_project && source.starts_with("project:") {
            continue;
        }
        let path_key = canonicalize_or_clone(&dir);
        if !seen_paths.insert(path_key) {
            continue;
        }
        let Ok(Some(pack)) = load_pack_from_dir(&dir, &source, shelf.as_deref()) else {
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

pub(crate) fn discover_packs(root: &Path) -> Vec<PackInfo> {
    discover_packs_with_project(root, true)
}

pub(crate) fn create_pack_from(
    root: &Path,
    selector: &str,
    project: bool,
    source_selector: Option<&str>,
) -> Result<PathBuf> {
    fn valid_segment(value: &str) -> bool {
        !value.is_empty()
            && value.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
    }

    let selector = selector.trim();
    let Some((shelf, name)) = selector.split_once('/') else {
        anyhow::bail!("pack location must be <shelf>/<name>");
    };
    if !valid_segment(shelf) || !valid_segment(name) || name.contains('/') {
        anyhow::bail!("shelf and pack names must use lowercase letters, digits, '-' or '_'");
    }
    let source = source_selector
        .map(|source| find_pack(root, source))
        .transpose()?;

    let shelf_root = if project {
        root.join(".dext/shelves")
    } else {
        dext_state_dir().join("shelves")
    };
    let pack_path = shelf_root.join(shelf).join("packs").join(name);
    let pack_path =
        crate::session::canonicalize_pack_scaffold_path(root, &pack_path.to_string_lossy())
            .map_err(anyhow::Error::msg)?;
    if let Some(source) = source.as_ref() {
        let source_path = canonicalize_or_clone(&source.path);
        if pack_path.starts_with(&source_path) {
            anyhow::bail!(
                "target pack path cannot be inside source pack: {}",
                source_path.display()
            );
        }
    }
    if std::fs::symlink_metadata(&pack_path).is_ok() {
        anyhow::bail!("pack path already exists: {}", pack_path.display());
    }
    let packs_path = pack_path
        .parent()
        .context("pack path is missing its packs directory")?;
    std::fs::create_dir_all(packs_path)
        .with_context(|| format!("creating shelf path {}", packs_path.display()))?;
    std::fs::create_dir(&pack_path)
        .with_context(|| format!("creating pack directory {}", pack_path.display()))?;
    let create_result = if let Some(source) = source {
        make_directory_private(&pack_path).and_then(|()| {
            copy_pack_dir(
                &source.path,
                &pack_path,
                Path::new(""),
                name,
                0,
                &mut PackCopyBudget::default(),
            )
        })
    } else {
        scaffold_pack(&pack_path, name)
    };
    if let Err(error) = create_result {
        return match std::fs::remove_dir_all(&pack_path) {
            Ok(()) => Err(error),
            Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(error),
            Err(cleanup) => Err(error).with_context(|| {
                format!(
                    "removing partial pack directory {} after copy failure: {cleanup}",
                    pack_path.display()
                )
            }),
        };
    }
    Ok(pack_path)
}

fn scaffold_pack(pack_path: &Path, name: &str) -> Result<()> {
    let title = name
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_ascii_uppercase().to_string() + chars.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ");
    let template = format!(
        "---\nname: {name}\ndescription: Describe what this pack adds to Dext.\n---\n\n# {title}\n\n## Use when\n\n- Describe the tasks this pack should handle.\n\n## Workflow\n\n1. Inspect the relevant project state.\n2. Perform the smallest useful action.\n3. Verify the result.\n\n## Output\n\n- Report changes, verification, and remaining gaps.\n"
    );
    let pack_md = pack_path.join("PACK.md");
    crate::session::atomic_write_bytes(&pack_md, template.as_bytes())
        .with_context(|| format!("writing {}", pack_md.display()))
}

fn make_directory_private(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("making copied pack directory private {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn copy_pack_dir(
    source: &Path,
    target: &Path,
    relative: &Path,
    name: &str,
    depth: usize,
    budget: &mut PackCopyBudget,
) -> Result<()> {
    if depth > PACK_FORK_DEPTH_CAP {
        anyhow::bail!(
            "source pack exceeds the {} directory-depth fork limit near {}",
            PACK_FORK_DEPTH_CAP,
            source.display()
        );
    }
    let source_metadata = std::fs::symlink_metadata(source)
        .with_context(|| format!("inspecting source pack directory {}", source.display()))?;
    if source_metadata.file_type().is_symlink() || !source_metadata.is_dir() {
        anyhow::bail!(
            "source pack entry is not a regular directory: {}",
            source.display()
        );
    }
    let entries = std::fs::read_dir(source)
        .with_context(|| format!("reading source pack directory {}", source.display()))?;
    let mut sorted_entries = Vec::new();
    for entry in entries {
        let entry = entry?;
        budget.add_entry(&entry.path())?;
        sorted_entries.push(entry);
    }
    sorted_entries.sort_by(|left, right| {
        let left_pack = relative.as_os_str().is_empty() && left.file_name() == "PACK.md";
        let right_pack = relative.as_os_str().is_empty() && right.file_name() == "PACK.md";
        right_pack
            .cmp(&left_pack)
            .then_with(|| left.file_name().cmp(&right.file_name()))
    });
    for entry in sorted_entries {
        let source_path = entry.path();
        let entry_relative = relative.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&source_path)
            .with_context(|| format!("inspecting source pack entry {}", source_path.display()))?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "source pack contains a symlink, which cannot be copied: {}",
                source_path.display()
            );
        }
        if fork_excluded_path(&entry_relative) {
            continue;
        }
        let target_path = target.join(entry.file_name());
        if metadata.is_dir() {
            std::fs::create_dir(&target_path)
                .with_context(|| format!("creating pack directory {}", target_path.display()))?;
            make_directory_private(&target_path)?;
            copy_pack_dir(
                &source_path,
                &target_path,
                &entry_relative,
                name,
                depth.saturating_add(1),
                budget,
            )?;
        } else if metadata.is_file() {
            if entry_relative == Path::new("PACK.md") {
                let workflow = read_pack_file(&source_path)?;
                if privacy_sensitive_content(workflow.as_bytes()) {
                    anyhow::bail!(
                        "source PACK.md contains content matched by privacy redaction rules"
                    );
                }
                let workflow = rewrite_pack_name(&workflow, name);
                budget.add_bytes(&source_path, workflow.len() as u64)?;
                crate::session::atomic_write_bytes(&target_path, workflow.as_bytes())
                    .with_context(|| format!("writing {}", target_path.display()))?;
            } else {
                copy_regular_file(&source_path, &target_path, budget)?;
            }
        } else {
            anyhow::bail!(
                "source pack contains a non-regular entry, which cannot be copied: {}",
                source_path.display()
            );
        }
    }
    Ok(())
}

fn fork_excluded_path(path: &Path) -> bool {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    file_name.starts_with(".env") || crate::privacy::privacy_sensitive_path(&path.to_string_lossy())
}

fn privacy_sensitive_content(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes)
        .ok()
        .is_some_and(|text| crate::privacy::redact_sensitive_text(text).counts.total() > 0)
}

fn copy_regular_file(source: &Path, target: &Path, budget: &mut PackCopyBudget) -> Result<()> {
    let (bytes, metadata) = crate::session::read_regular_file_bytes_with_limit(
        source,
        PACK_FORK_FILE_CAP as usize,
        None,
        "pack fork file",
    )
    .map_err(anyhow::Error::msg)?;
    budget.add_bytes(source, bytes.len() as u64)?;
    if privacy_sensitive_content(&bytes) {
        return Ok(());
    }

    let mut permissions = metadata.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        permissions.set_mode(permissions.mode() & 0o777);
    }
    crate::session::atomic_write_bytes_with_permissions(target, &bytes, &permissions)
        .with_context(|| format!("copying {} to {}", source.display(), target.display()))?;
    Ok(())
}

fn rewrite_pack_name(workflow: &str, name: &str) -> String {
    let newline = if workflow.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let first_end = workflow
        .find('\n')
        .map(|index| index + 1)
        .unwrap_or(workflow.len());
    if workflow[..first_end].trim() != "---" {
        return format!("---{newline}name: {name}{newline}---{newline}{newline}{workflow}");
    }

    let mut out = String::with_capacity(workflow.len().saturating_add(name.len() + 8));
    out.push_str(&workflow[..first_end]);
    let mut cursor = first_end;
    let mut replaced = false;
    while cursor < workflow.len() {
        let end = workflow[cursor..]
            .find('\n')
            .map(|index| cursor + index + 1)
            .unwrap_or(workflow.len());
        let line = &workflow[cursor..end];
        let body = line.trim_end_matches(['\r', '\n']);
        let ending = &line[body.len()..];
        let trimmed = body.trim();
        if trimmed == "---" {
            if !replaced {
                out.push_str(&format!("name: {name}{newline}"));
            }
            out.push_str(&workflow[cursor..]);
            return out;
        }
        if trimmed
            .split_once(':')
            .is_some_and(|(key, _)| key.trim().eq_ignore_ascii_case("name"))
        {
            out.push_str(&format!("name: {name}{ending}"));
            replaced = true;
        } else {
            out.push_str(line);
        }
        cursor = end;
    }

    if !replaced {
        let rest = out.split_off(first_end);
        out.push_str(&format!("name: {name}{newline}"));
        out.push_str(&rest);
    }
    out
}

pub(crate) fn find_pack_exact_source(root: &Path, name: &str, source: &str) -> Result<PackInfo> {
    let key = normalize_key(name);
    let mut seen_paths = HashSet::new();
    for (dir, candidate_source, shelf) in candidate_pack_dirs(root) {
        if pack_source_identity(&candidate_source, &dir) != source
            || !seen_paths.insert(canonicalize_or_clone(&dir))
        {
            continue;
        }
        let Some(pack) = load_pack_from_dir(&dir, &candidate_source, shelf.as_deref())? else {
            continue;
        };
        if normalize_key(&pack.name) == key {
            return Ok(pack);
        }
    }
    anyhow::bail!("pack '{name}' from saved source '{source}' not found")
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

#[allow(dead_code)] // test convenience wrapper
pub(crate) fn render_pack_listing(root: &Path) -> String {
    render_pack_listing_opts(root, false)
}

pub(crate) fn render_pack_listing_opts(root: &Path, verbose: bool) -> String {
    render_pack_listing_opts_width(root, verbose, None)
}

pub(crate) fn render_pack_listing_json(root: &Path) -> Result<String> {
    serde_json::to_string(&discover_packs(root)).context("serializing pack catalog")
}

pub(crate) fn render_pack_listing_opts_width(
    root: &Path,
    verbose: bool,
    width: Option<usize>,
) -> String {
    render_pack_list(
        &discover_packs(root),
        &list_render::ListOptions::detect_with_width(verbose, width),
        root,
    )
}

/// Pure list renderer over discovered packs. Discovery/loading stays unchanged.
pub(crate) fn render_pack_list(
    packs: &[PackInfo],
    opts: &list_render::ListOptions,
    root: &Path,
) -> String {
    use std::fmt::Write as _;
    if packs.is_empty() {
        let mut out = list_render::render_header("Packs", 0, opts);
        out.push_str(&list_render::render_section_header("Search paths", opts));
        list_render::write_wrapped(
            &mut out,
            ".dext/shelves/*/packs, DEXT_SHELVES_DIR, ~/.dext/shelves/*/packs",
            2,
            opts.effective_width(),
        );
        return out.trim_end().to_string();
    }
    let mut out = String::new();
    let _ = write!(
        out,
        "{}",
        list_render::render_header("Packs", packs.len(), opts)
    );
    for pack in packs.iter().take(PACK_LIST_LIMIT) {
        let shelf = pack.shelf.clone().unwrap_or_else(|| "none".to_string());
        let mut meta: Vec<(&str, String)> = vec![
            ("source", compact_source(&pack.source, opts)),
            ("shelf", shelf),
        ];
        if opts.verbose {
            meta.push(("path", list_render::display_path(&pack.path, opts, root)));
        }
        let desc = if pack.description.trim().is_empty() {
            "(no description)"
        } else {
            pack.description.as_str()
        };
        out.push_str(&list_render::render_entry(&pack.name, desc, &meta, opts));
    }
    if packs.len() > PACK_LIST_LIMIT {
        list_render::write_wrapped(
            &mut out,
            &format!("… [{} more packs omitted]", packs.len() - PACK_LIST_LIMIT),
            2,
            opts.effective_width(),
        );
    }
    out.push_str(&list_render::render_footer(
        &["/pack inspect <name>", "/pack run <name> <task>"],
        opts,
    ));
    out
}

/// Extract the scope prefix from a source label (`project:...` → `project`,
/// `user:...` → `user`, `env:...` → `env`).
/// In verbose mode, returns the full source string unchanged.
fn compact_source(source: &str, opts: &list_render::ListOptions) -> String {
    if opts.verbose {
        return source.to_string();
    }
    match source.split_once(':') {
        Some((scope, _)) => scope.to_string(),
        None => source.to_string(),
    }
}

fn render_pack_ui(ui: &PackUi) -> Result<String> {
    let json = serde_json::to_string_pretty(ui).context("serializing pack UI metadata")?;
    Ok(format!("ui: {}", json.replace('\n', "\n  ")))
}

pub(crate) fn render_pack_inspect(root: &Path, selector: &str) -> Result<String> {
    let pack = find_pack(root, selector)?;
    let hooks = pack
        .phooks_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(none)".to_string());
    let runtime = pack
        .runtime_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(none)".to_string());
    let credentials = if pack.credential_env_ignored {
        "(ignored: project-local packs cannot inherit parent credentials)".to_string()
    } else if pack.credential_env.is_empty() {
        "(none)".to_string()
    } else {
        pack.credential_env.join(", ")
    };
    let mut out = format!(
        "pack: {}\ndescription: {}\nshelf: {}\nsource: {}\npath: {}\nworkflow: {}\nhooks: {}\nruntime: {}\nenv: {}={}\ncredential env: {}",
        pack.name,
        if pack.description.is_empty() {
            "(none)"
        } else {
            &pack.description
        },
        pack.shelf.as_deref().unwrap_or("(none)"),
        pack.source,
        pack.path.display(),
        pack.pack_md_path.display(),
        hooks,
        runtime,
        pack.env_var_name(),
        pack.path.display(),
        credentials,
    );
    if let Some(ui) = &pack.ui {
        out.push('\n');
        out.push_str(&render_pack_ui(ui)?);
    }
    Ok(out)
}

pub(crate) fn pack_prompt(pack: &PackInfo, task: &str) -> Result<String> {
    let workflow = read_pack_file(&pack.pack_md_path)?;
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
        "[dext pack invocation]\nPack: {name}\nDescription: {description}\nShelf: {shelf}\nPack path: {path}\nWorkflow: {workflow_path}\n{hook_line}\nPack env: {env_name}={path}\n\nFollow the PACK.md workflow below. Treat this as an explicit user request to invoke the pack; do not just describe how to run it. Use normal Dext tools and pack-local helper scripts through bash when the workflow says to.\n\n--- PACK.md ---\n{workflow}\n--- END PACK.md ---\n\nUser task for this pack:\n{task}",
        name = pack.name,
        description = if pack.description.is_empty() {
            "(none)"
        } else {
            &pack.description
        },
        shelf = pack.shelf.as_deref().unwrap_or("(none)"),
        path = pack.path.display(),
        workflow_path = pack.pack_md_path.display(),
        env_name = pack.env_var_name(),
    ))
}

pub(crate) fn pack_invocation_args(raw: &str) -> Option<(&str, &str)> {
    let raw = raw.trim();
    let (first, rest) = raw.split_once(char::is_whitespace)?;
    let rest = rest.trim();
    if matches!(first, "run" | "use" | "start") {
        let (selector, task) = rest.split_once(char::is_whitespace)?;
        let task = task.trim();
        (!selector.is_empty() && !task.is_empty()).then_some((selector, task))
    } else if matches!(
        first,
        "list" | "ls" | "inspect" | "info" | "show" | "create" | "new"
    ) {
        None
    } else {
        (!rest.is_empty()).then_some((first, rest))
    }
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

pub(crate) fn project_pack_invocation_requested(root: &Path, user_input: &str) -> bool {
    let text = normalized_words(user_input);
    if text.contains("what is ") || text.contains("explain ") {
        return false;
    }
    candidate_pack_dirs(root)
        .into_iter()
        .filter(|(_, source, _)| source.starts_with("project:"))
        .filter_map(|(dir, source, shelf)| {
            load_pack_from_dir(&dir, &source, shelf.as_deref())
                .ok()
                .flatten()
        })
        .any(|pack| has_invocation_pattern(&text, &normalize_key(&pack.name)))
}

pub(crate) fn infer_pack_invocation_with_project(
    root: &Path,
    user_input: &str,
    include_project: bool,
) -> Option<PackInvocation> {
    let text = normalized_words(user_input);
    if text.contains("what is ") || text.contains("explain ") {
        return None;
    }
    let mut packs = discover_packs_with_project(root, include_project);
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

#[cfg(test)]
pub(crate) fn infer_pack_invocation(root: &Path, user_input: &str) -> Option<PackInvocation> {
    infer_pack_invocation_with_project(root, user_input, true)
}

pub(crate) fn pack_summary_for_prompt(root: &Path, include_project: bool) -> Option<String> {
    let packs = discover_packs_with_project(root, include_project);
    if packs.is_empty() {
        return None;
    }
    let mut out = String::from("Available Dext packs: ");
    out.push_str(
        &packs
            .iter()
            .take(10)
            .map(|pack| {
                let name = crate::summarize_inline(&pack.name, 96);
                match pack.shelf.as_deref() {
                    Some(shelf) => format!("{name}[{}]", crate::summarize_inline(shelf, 64)),
                    None => name,
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
    );
    if packs.len() > 10 {
        out.push_str(&format!(", … +{}", packs.len() - 10));
    }
    out.push_str(". Invoke with `/pack run <name> <task>` or `dext pack run <name> <task>`. Create packs with `/pack create <shelf>/<name>` or `dext pack create <shelf>/<name>`.");
    Some(byte_prefix_at_char_boundary(&out, 1_000).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rewrite_handles_duplicate_missing_and_absent_front_matter() {
        for (source, expected) in [
            (
                "---\nname: old\ndescription: demo\nNAME: stale\n---\n# Body\n",
                "---\nname: new\ndescription: demo\nname: new\n---\n# Body\n",
            ),
            (
                "---\ndescription: demo\n---\n# Body\n",
                "---\ndescription: demo\nname: new\n---\n# Body\n",
            ),
            ("# Body\n", "---\nname: new\n---\n\n# Body\n"),
        ] {
            assert_eq!(rewrite_pack_name(source, "new"), expected);
        }
    }

    #[test]
    fn copy_budget_and_depth_limits_fail_closed() {
        let root = std::env::temp_dir().join(format!(
            "dext-pack-copy-limits-{}-{}",
            std::process::id(),
            crate::unix_timestamp_secs()
        ));
        let source = root.join("source");
        let target = root.join("target");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("one"), "one").unwrap();
        let entry_error = copy_pack_dir(
            &source,
            &target,
            Path::new(""),
            "fork",
            0,
            &mut PackCopyBudget {
                entries: PACK_FORK_ENTRY_CAP,
                bytes: 0,
            },
        )
        .unwrap_err();
        assert!(entry_error.to_string().contains("entry fork limit"));
        let depth_error = copy_pack_dir(
            &source,
            &target,
            Path::new(""),
            "fork",
            PACK_FORK_DEPTH_CAP + 1,
            &mut PackCopyBudget::default(),
        )
        .unwrap_err();
        assert!(
            depth_error
                .to_string()
                .contains("directory-depth fork limit")
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
