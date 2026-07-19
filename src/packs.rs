use anyhow::{Context, Result};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::list_render;
use crate::session::{atomic_write_bytes, canonicalize_or_clone, dext_state_dir};
use crate::{byte_prefix_at_char_boundary, cap_bytes_with_hint};

const PACK_PROMPT_CAP: usize = 32_000;
const PACK_LIST_LIMIT: usize = 50;

type PackDirCandidate = (PathBuf, String, Option<String>);

struct BundledPackFile {
    relative_path: &'static str,
    bytes: &'static [u8],
    executable: bool,
}

const BUNDLED_PACK_FILES: &[BundledPackFile] = &[
    BundledPackFile {
        relative_path: "agent-browser/PACK.md",
        bytes: include_bytes!("../packs/agent-browser/PACK.md"),
        executable: false,
    },
    BundledPackFile {
        relative_path: "agent-browser/bin/agent-browser",
        bytes: include_bytes!("../packs/agent-browser/bin/agent-browser"),
        executable: true,
    },
    BundledPackFile {
        relative_path: "autoresearch/PACK.md",
        bytes: include_bytes!("../packs/autoresearch/PACK.md"),
        executable: false,
    },
    BundledPackFile {
        relative_path: "autoresearch/bin/autoresearch.py",
        bytes: include_bytes!("../packs/autoresearch/bin/autoresearch.py"),
        executable: true,
    },
    BundledPackFile {
        relative_path: "autoresearch/hooks/post_bash.py",
        bytes: include_bytes!("../packs/autoresearch/hooks/post_bash.py"),
        executable: true,
    },
    BundledPackFile {
        relative_path: "autoresearch/hooks/user_prompt.py",
        bytes: include_bytes!("../packs/autoresearch/hooks/user_prompt.py"),
        executable: true,
    },
    BundledPackFile {
        relative_path: "autoresearch/phooks.json",
        bytes: include_bytes!("../packs/autoresearch/phooks.json"),
        executable: false,
    },
    BundledPackFile {
        relative_path: "packopt/PACK.md",
        bytes: include_bytes!("../packs/packopt/PACK.md"),
        executable: false,
    },
    BundledPackFile {
        relative_path: "packopt/bin/packopt.py",
        bytes: include_bytes!("../packs/packopt/bin/packopt.py"),
        executable: true,
    },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackInfo {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) path: PathBuf,
    pub(crate) pack_md_path: PathBuf,
    pub(crate) phooks_path: Option<PathBuf>,
    pub(crate) credential_env: Vec<String>,
    pub(crate) credential_env_ignored: bool,
    pub(crate) source: String,
    pub(crate) shelf: Option<String>,
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
            "credential-env" | "credential_env" => {
                front.credential_env.extend(parse_env_names(value));
                front.credential_env.sort();
                front.credential_env.dedup();
                front.credential_env.truncate(32);
            }
            _ => {}
        }
    }
    front
}

fn load_pack_from_dir(dir: &Path, source: &str, shelf: Option<&str>) -> Result<Option<PackInfo>> {
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
    let credential_env_ignored = source.starts_with("project:") && !front.credential_env.is_empty();
    let credential_env = if credential_env_ignored {
        Vec::new()
    } else {
        front.credential_env
    };
    let path = dir.to_path_buf();
    let phooks = path.join("phooks.json");
    Ok(Some(PackInfo {
        name,
        description,
        path,
        pack_md_path,
        phooks_path: phooks.is_file().then_some(phooks),
        credential_env,
        credential_env_ignored,
        source: source.to_string(),
        shelf: shelf.map(str::to_string),
    }))
}

fn push_pack_root(dirs: &mut Vec<PackDirCandidate>, pack_root: PathBuf, label: impl Into<String>) {
    if !pack_root.is_dir() {
        return;
    }
    let label = label.into();
    push_direct(dirs, pack_root.clone(), label.clone(), None);
    let Ok(entries) = std::fs::read_dir(&pack_root) else {
        return;
    };
    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let path = entry.path();
        if path.join("PACK.md").is_file() {
            push_direct(dirs, path, label.clone(), None);
        }
    }
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

fn bundled_pack_digest() -> String {
    let mut hasher = Sha256::new();
    for file in BUNDLED_PACK_FILES {
        hasher.update(file.relative_path.as_bytes());
        hasher.update([0]);
        hasher.update(file.bytes);
        hasher.update([file.executable as u8]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_cache_parent(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspecting Dext state directory {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!(
            "Dext state path must be a real directory: {}",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.uid() != unsafe { libc::geteuid() } {
            anyhow::bail!(
                "Dext state directory is not owned by the current user: {}",
                path.display()
            );
        }
        let mode = metadata.mode() & 0o777;
        if mode & 0o022 != 0 {
            anyhow::bail!(
                "Dext state directory has unsafe writable mode {mode:04o}: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn ensure_private_cache_dir(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                anyhow::bail!(
                    "bundled pack cache path must be a real directory: {}",
                    path.display()
                );
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                if metadata.uid() != unsafe { libc::geteuid() } {
                    anyhow::bail!(
                        "bundled pack cache directory is not owned by the current user: {}",
                        path.display()
                    );
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            builder.create(path).with_context(|| {
                format!("creating bundled pack cache directory {}", path.display())
            })?;
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspecting bundled pack cache directory {}", path.display())
            });
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting bundled pack cache mode on {}", path.display()))?;
    }
    Ok(())
}

fn ensure_private_cache_tree(base: &Path, target: &Path) -> Result<()> {
    ensure_private_cache_dir(base)?;
    let relative = target.strip_prefix(base).with_context(|| {
        format!(
            "bundled pack cache path {} escapes {}",
            target.display(),
            base.display()
        )
    })?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            anyhow::bail!("unsafe bundled pack cache path: {}", target.display());
        };
        current.push(name);
        ensure_private_cache_dir(&current)?;
    }
    Ok(())
}

fn materialize_bundled_packs() -> Result<PathBuf> {
    let state_dir = dext_state_dir();
    let state_dir_created = match std::fs::symlink_metadata(&state_dir) {
        Ok(_) => false,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(&state_dir).with_context(|| {
                format!("creating Dext state directory {}", state_dir.display())
            })?;
            true
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("inspecting Dext state directory {}", state_dir.display())
            });
        }
    };
    if state_dir_created {
        ensure_private_cache_dir(&state_dir)?;
    } else {
        validate_cache_parent(&state_dir)?;
    }
    let state_dir = std::fs::canonicalize(&state_dir)
        .with_context(|| format!("resolving Dext state directory {}", state_dir.display()))?;
    let cache_root = state_dir.join("bundled-packs");
    ensure_private_cache_dir(&cache_root)?;
    let root = cache_root.join(bundled_pack_digest());
    ensure_private_cache_dir(&root)?;
    for bundled in BUNDLED_PACK_FILES {
        let path = root.join(bundled.relative_path);
        let parent = path
            .parent()
            .context("bundled pack file is missing a parent directory")?;
        ensure_private_cache_tree(&root, parent)?;
        let matches = std::fs::symlink_metadata(&path)
            .ok()
            .filter(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
            .and_then(|_| std::fs::read(&path).ok())
            .is_some_and(|bytes| bytes == bundled.bytes);
        if !matches {
            atomic_write_bytes(&path, bundled.bytes)
                .with_context(|| format!("materializing bundled pack file {}", path.display()))?;
        }
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("validating bundled pack file {}", path.display()))?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || std::fs::read(&path).ok().as_deref() != Some(bundled.bytes)
        {
            anyhow::bail!("bundled pack cache validation failed: {}", path.display());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = if bundled.executable { 0o700 } else { 0o600 };
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .with_context(|| format!("setting bundled pack mode on {}", path.display()))?;
        }
    }
    Ok(root)
}

fn candidate_pack_dirs(root: &Path) -> (Vec<PackDirCandidate>, Option<String>) {
    let mut direct = Vec::new();
    for (key, value) in std::env::vars() {
        if key.starts_with("DEXT_PACK_") && key.ends_with("_DIR") && !value.trim().is_empty() {
            push_direct(
                &mut direct,
                PathBuf::from(value),
                format!("env:{key}"),
                None,
            );
        }
    }

    push_shelf_root(
        &mut direct,
        root.join(".dext/shelves"),
        "project:.dext/shelves",
    );
    push_pack_root(&mut direct, root.join(".dext/packs"), "project:.dext/packs");
    push_pack_root(&mut direct, root.join("packs"), "project:packs");

    if let Some(paths) = std::env::var_os("DEXT_SHELVES_DIR") {
        for path in std::env::split_paths(&paths) {
            push_shelf_root(&mut direct, path, "env:DEXT_SHELVES_DIR");
        }
    }

    if let Some(paths) = std::env::var_os("DEXT_PACKS_DIR") {
        for path in std::env::split_paths(&paths) {
            push_pack_root(&mut direct, path, "env:DEXT_PACKS_DIR");
        }
    }
    push_shelf_root(
        &mut direct,
        dext_state_dir().join("shelves"),
        "user:~/.dext/shelves",
    );
    push_pack_root(
        &mut direct,
        dext_state_dir().join("packs"),
        "user:~/.dext/packs",
    );
    let bundled_error = match materialize_bundled_packs() {
        Ok(bundled_packs) => {
            push_pack_root(&mut direct, bundled_packs, "bundled:embedded");
            None
        }
        Err(error) => Some(format!("bundled packs unavailable: {error:#}")),
    };

    (direct, bundled_error)
}

fn discover_packs_with_warning(root: &Path) -> (Vec<PackInfo>, Option<String>) {
    let mut packs = Vec::new();
    let mut seen_names = HashSet::new();
    let mut seen_paths = HashSet::new();
    let (candidate_dirs, warning) = candidate_pack_dirs(root);
    for (dir, source, shelf) in candidate_dirs {
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
    (packs, warning)
}

pub(crate) fn discover_packs(root: &Path) -> Vec<PackInfo> {
    discover_packs_with_warning(root).0
}

pub(crate) fn find_pack(root: &Path, selector: &str) -> Result<PackInfo> {
    let selector = selector.trim();
    if selector.is_empty() {
        anyhow::bail!("missing pack name");
    }
    let key = normalize_key(selector);
    let (packs, bundled_warning) = discover_packs_with_warning(root);
    if let Some(pack) = packs.iter().find(|pack| normalize_key(&pack.name) == key) {
        return Ok(pack.clone());
    }
    let matches: Vec<PackInfo> = packs
        .into_iter()
        .filter(|pack| normalize_key(&pack.name).starts_with(&key))
        .collect();
    match matches.as_slice() {
        [pack] => Ok(pack.clone()),
        [] => {
            if let Some(warning) = bundled_warning {
                anyhow::bail!("pack '{selector}' not found; {warning}");
            }
            anyhow::bail!("pack '{selector}' not found. Run /pack list.")
        }
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
    let (packs, warning) = discover_packs_with_warning(root);
    let mut listing = render_pack_list(&packs, &list_render::ListOptions::detect(verbose), root);
    if let Some(warning) = warning {
        listing.push_str("\nwarning: ");
        listing.push_str(&warning);
    }
    listing
}

/// Pure list renderer over discovered packs. Discovery/loading stays unchanged.
pub(crate) fn render_pack_list(
    packs: &[PackInfo],
    opts: &list_render::ListOptions,
    root: &Path,
) -> String {
    use std::fmt::Write as _;
    if packs.is_empty() {
        return "Packs  none found\nsearch paths: .dext/shelves/*/packs, .dext/packs, packs, DEXT_SHELVES_DIR, DEXT_PACKS_DIR, ~/.dext/shelves/*/packs, ~/.dext/packs, bundled packs".to_string();
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
        let _ = writeln!(
            out,
            "  … [{} more packs omitted]",
            packs.len() - PACK_LIST_LIMIT
        );
    }
    out.push_str(&list_render::render_footer(
        &["/pack inspect <name>", "/pack run <name> <task>"],
        opts,
    ));
    out
}

/// Extract the scope prefix from a source label (`project:...` → `project`,
/// `user:...` → `user`, `env:...` → `env`, `bundled:...` → `bundled`).
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

pub(crate) fn render_pack_inspect(root: &Path, selector: &str) -> Result<String> {
    let pack = find_pack(root, selector)?;
    let hooks = pack
        .phooks_path
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
    Ok(format!(
        "pack: {}\ndescription: {}\nshelf: {}\nsource: {}\npath: {}\nworkflow: {}\nhooks: {}\nenv: {}={}\ncredential env: {}",
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
        pack.env_var_name(),
        pack.path.display(),
        credentials
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
    } else if matches!(first, "list" | "ls" | "inspect" | "info" | "show") {
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
            .map(|pack| match pack.shelf.as_deref() {
                Some(shelf) => format!("{}[{shelf}]", pack.name),
                None => pack.name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", "),
    );
    if packs.len() > 10 {
        out.push_str(&format!(", … +{}", packs.len() - 10));
    }
    out.push_str(". Invoke with `/pack run <name> <task>`, `dext pack run <name> <task>`, or conversationally (for example, 'run autoresearch on …').");
    Some(byte_prefix_at_char_boundary(&out, 1_000).to_string())
}
