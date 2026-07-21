use crate::session::{canonicalize_mutation_parent_path, canonicalize_mutation_path};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const PREVIEW_DIFF_CAP: usize = 4096;
const MATCH_DISPLAY_LIMIT: usize = 8;
static TEMP_ORDINAL: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExpectedFile {
    Missing,
    Regular(FileFingerprint),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFingerprint {
    len: u64,
    modified_ns: Option<u128>,
    sha256: [u8; 32],
    mode: Option<u32>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedMutation {
    requested_path: String,
    path: PathBuf,
    expected: ExpectedFile,
    before: Vec<u8>,
    after: Vec<u8>,
}

impl PreparedMutation {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn is_new_file(&self) -> bool {
        matches!(self.expected, ExpectedFile::Missing)
    }

    pub(crate) fn before_text(&self) -> &str {
        std::str::from_utf8(&self.before).expect("native text mutation prepared from UTF-8")
    }

    pub(crate) fn after_text(&self) -> &str {
        std::str::from_utf8(&self.after).expect("native text mutation prepared from UTF-8")
    }

    pub(crate) fn preview(&self) -> MutationPreview {
        compute_preview(
            self.path.clone(),
            self.before_text(),
            self.after_text(),
            self.is_new_file(),
        )
    }
}

pub(crate) struct MutationPreview {
    pub(crate) path: PathBuf,
    pub(crate) is_new_file: bool,
    pub(crate) added: usize,
    pub(crate) removed: usize,
    pub(crate) diff: String,
    pub(crate) truncated: bool,
}

pub(crate) struct MultiEdit {
    pub(crate) old_string: String,
    pub(crate) new_string: String,
    pub(crate) replace_all: bool,
}

pub(crate) fn prepare_tool_mutation(
    name: &str,
    input: &Value,
    root: &Path,
) -> Result<Option<PreparedMutation>, String> {
    match name {
        "write_file" => {
            let path = input["path"].as_str().ok_or("missing path")?;
            let content = input["content"].as_str().ok_or("missing content")?;
            prepare_write_file(root, path, content).map(Some)
        }
        "edit_file" => {
            let path = input["path"].as_str().ok_or("missing path")?;
            let old = input["old_string"].as_str().ok_or("missing old_string")?;
            let new = input["new_string"].as_str().ok_or("missing new_string")?;
            prepare_edit_file(root, path, old, new).map(Some)
        }
        "multi_edit" => {
            let path = input["path"].as_str().ok_or("missing path")?;
            let edits = input["edits"].as_array().ok_or("missing edits array")?;
            let edits = edits
                .iter()
                .enumerate()
                .map(|(index, edit)| {
                    Ok(MultiEdit {
                        old_string: edit["old_string"]
                            .as_str()
                            .ok_or_else(|| format!("edit[{index}]: missing old_string"))?
                            .to_string(),
                        new_string: edit["new_string"]
                            .as_str()
                            .ok_or_else(|| format!("edit[{index}]: missing new_string"))?
                            .to_string(),
                        replace_all: edit["replace_all"].as_bool().unwrap_or(false),
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            prepare_multi_edit(root, path, &edits).map(Some)
        }
        _ => Ok(None),
    }
}

pub(crate) fn prepare_write_file(
    root: &Path,
    path_str: &str,
    content: &str,
) -> Result<PreparedMutation, String> {
    let path = canonicalize_mutation_path(root, path_str)?;
    let (expected, before) = capture_expected(&path)?;
    Ok(PreparedMutation {
        requested_path: path_str.to_string(),
        path,
        expected,
        before,
        after: content.as_bytes().to_vec(),
    })
}

pub(crate) fn prepare_edit_file(
    root: &Path,
    path_str: &str,
    old: &str,
    new: &str,
) -> Result<PreparedMutation, String> {
    let path = canonicalize_mutation_path(root, path_str)?;
    let (expected, before) = capture_expected(&path)?;
    if matches!(expected, ExpectedFile::Missing) {
        return Err(format!("failed to read {}: file not found", path.display()));
    }
    let content = std::str::from_utf8(&before)
        .map_err(|error| format!("failed to read {} as UTF-8: {error}", path.display()))?;
    let count = content.matches(old).count();
    if count == 0 {
        return Err(format!("old_string not found in {}", path.display()));
    }
    if count > 1 {
        return Err(render_match_locations(&path, root, content, old, count));
    }
    let after = content.replacen(old, new, 1).into_bytes();
    Ok(PreparedMutation {
        requested_path: path_str.to_string(),
        path,
        expected,
        before,
        after,
    })
}

pub(crate) fn prepare_multi_edit(
    root: &Path,
    path_str: &str,
    edits: &[MultiEdit],
) -> Result<PreparedMutation, String> {
    let path = canonicalize_mutation_path(root, path_str)?;
    let (expected, before) = capture_expected(&path)?;
    if matches!(expected, ExpectedFile::Missing) {
        return Err(format!("failed to read {}: file not found", path.display()));
    }
    let before_text = std::str::from_utf8(&before)
        .map_err(|error| format!("failed to read {} as UTF-8: {error}", path.display()))?;
    let mut content = before_text.to_string();
    for (index, edit) in edits.iter().enumerate() {
        if edit.replace_all {
            if !content.contains(&edit.old_string) {
                return Err(format!("edit[{index}]: old_string not found"));
            }
            content = content.replace(&edit.old_string, &edit.new_string);
        } else {
            let count = content.matches(&edit.old_string).count();
            if count == 0 {
                return Err(format!("edit[{index}]: old_string not found"));
            }
            if count > 1 {
                return Err(format!(
                    "edit[{index}]: {}",
                    render_match_locations(&path, root, &content, &edit.old_string, count)
                ));
            }
            content = content.replacen(&edit.old_string, &edit.new_string, 1);
        }
    }
    Ok(PreparedMutation {
        requested_path: path_str.to_string(),
        path,
        expected,
        before,
        after: content.into_bytes(),
    })
}

pub(crate) fn apply_prepared_mutation(
    root: &Path,
    prepared: &PreparedMutation,
) -> Result<(), String> {
    apply_prepared_mutation_with_hook(root, prepared, |_| Ok(()))
}

fn apply_prepared_mutation_with_hook(
    root: &Path,
    prepared: &PreparedMutation,
    before_replace: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let parent = prepared
        .path
        .parent()
        .ok_or_else(|| format!("mutation path has no parent: {}", prepared.path.display()))?;
    validate_destination(root, prepared)?;
    ensure_parent_directories(root, prepared, parent)?;
    validate_destination(root, prepared)?;

    let temp = create_temp_file(parent, &prepared.path, &prepared.expected)?;
    let write_result = (|| -> Result<(), String> {
        let mut file = temp.1;
        file.write_all(&prepared.after)
            .map_err(|error| format!("writing mutation temp {}: {error}", temp.0.display()))?;
        apply_preserved_permissions(&file, &prepared.expected)?;
        file.sync_all()
            .map_err(|error| format!("syncing mutation temp {}: {error}", temp.0.display()))?;
        drop(file);

        before_replace(&temp.0)?;
        validate_destination(root, prepared)?;
        replace_file_atomically(&temp.0, &prepared.path).map_err(|error| {
            format!(
                "atomically replacing {} from {}: {error}",
                prepared.path.display(),
                temp.0.display()
            )
        })?;
        sync_parent_best_effort(parent);
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp.0);
    }
    write_result
}

#[cfg(test)]
pub(crate) fn fail_prepared_mutation_before_replace(
    root: &Path,
    prepared: &PreparedMutation,
) -> Result<(), String> {
    apply_prepared_mutation_with_hook(root, prepared, |_| {
        Err("injected pre-replacement failure".to_string())
    })
}

fn capture_expected(path: &Path) -> Result<(ExpectedFile, Vec<u8>), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((ExpectedFile::Missing, Vec::new()));
        }
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "mutation destination is a symlink: {}",
            path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "mutation destination is not a regular file: {}",
            path.display()
        ));
    }

    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let file_metadata = file
        .metadata()
        .map_err(|error| format!("failed to inspect open file {}: {error}", path.display()))?;
    let expected_fingerprint = fingerprint(&file_metadata, &bytes);
    let current = std::fs::symlink_metadata(path)
        .map_err(|error| format!("file changed while preparing {}: {error}", path.display()))?;
    if current.file_type().is_symlink()
        || !current.is_file()
        || fingerprint(&current, &bytes) != expected_fingerprint
    {
        return Err(format!(
            "file changed while preparing {}; re-read and retry",
            path.display()
        ));
    }
    Ok((ExpectedFile::Regular(expected_fingerprint), bytes))
}

fn ensure_parent_directories(
    root: &Path,
    prepared: &PreparedMutation,
    parent: &Path,
) -> Result<(), String> {
    let mut missing = Vec::new();
    let mut cursor = parent;
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(stale_error(
                        prepared,
                        &format!(
                            "parent component is not a real directory: {}",
                            cursor.display()
                        ),
                    ));
                }
                let resolved = canonicalize_mutation_parent_path(root, &cursor.to_string_lossy())
                    .map_err(|error| {
                    stale_error(
                        prepared,
                        &format!(
                            "parent component no longer resolves safely ({}): {error}",
                            cursor.display()
                        ),
                    )
                })?;
                if resolved != cursor {
                    return Err(stale_error(
                        prepared,
                        &format!("parent component changed: {}", cursor.display()),
                    ));
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = cursor.file_name().ok_or_else(|| {
                    stale_error(
                        prepared,
                        &format!("parent path has no directory name: {}", cursor.display()),
                    )
                })?;
                missing.push(name.to_os_string());
                cursor = cursor.parent().ok_or_else(|| {
                    stale_error(
                        prepared,
                        &format!("parent path has no existing ancestor: {}", parent.display()),
                    )
                })?;
            }
            Err(error) => {
                return Err(stale_error(
                    prepared,
                    &format!(
                        "cannot inspect parent component {}: {error}",
                        cursor.display()
                    ),
                ));
            }
        }
    }

    let mut current = cursor.to_path_buf();
    for name in missing.into_iter().rev() {
        current.push(name);
        match std::fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "creating parent directory {}: {error}",
                    current.display()
                ));
            }
        }
        let metadata = std::fs::symlink_metadata(&current).map_err(|error| {
            stale_error(
                prepared,
                &format!(
                    "cannot inspect created parent directory {}: {error}",
                    current.display()
                ),
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(stale_error(
                prepared,
                &format!(
                    "created parent component is not a real directory: {}",
                    current.display()
                ),
            ));
        }
        let resolved = canonicalize_mutation_parent_path(root, &current.to_string_lossy())
            .map_err(|error| {
                stale_error(
                    prepared,
                    &format!(
                        "created parent component does not resolve safely ({}): {error}",
                        current.display()
                    ),
                )
            })?;
        if resolved != current {
            return Err(stale_error(
                prepared,
                &format!("created parent component changed: {}", current.display()),
            ));
        }
    }
    Ok(())
}

fn validate_destination(root: &Path, prepared: &PreparedMutation) -> Result<(), String> {
    let resolved = canonicalize_mutation_path(root, &prepared.requested_path).map_err(|error| {
        stale_error(
            prepared,
            &format!("the destination path no longer resolves safely: {error}"),
        )
    })?;
    if resolved != prepared.path {
        return Err(stale_error(
            prepared,
            "the resolved destination path changed",
        ));
    }
    let current = capture_expected(&prepared.path)
        .map_err(|error| stale_error(prepared, &format!("destination is unsafe: {error}")))?;
    if current.0 != prepared.expected {
        return Err(stale_error(
            prepared,
            "the destination existence, type, length, modification identity, permissions, or content changed",
        ));
    }
    Ok(())
}

fn stale_error(prepared: &PreparedMutation, reason: &str) -> String {
    format!(
        "stale file state for {}: {reason}; nothing was written. Re-read the file and retry the mutation.",
        prepared.path.display()
    )
}

fn fingerprint(metadata: &std::fs::Metadata, bytes: &[u8]) -> FileFingerprint {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let digest = Sha256::digest(bytes);
    let mut sha256 = [0u8; 32];
    sha256.copy_from_slice(&digest);
    FileFingerprint {
        len: metadata.len(),
        modified_ns: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos()),
        sha256,
        #[cfg(unix)]
        mode: Some(metadata.permissions().mode() & 0o777),
        #[cfg(not(unix))]
        mode: None,
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    }
}

fn create_temp_file(
    parent: &Path,
    destination: &Path,
    expected: &ExpectedFile,
) -> Result<(PathBuf, std::fs::File), String> {
    #[cfg(not(unix))]
    let _ = expected;
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("mutation");
    for _ in 0..16 {
        let ordinal = TEMP_ORDINAL.fetch_add(1, Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = parent.join(format!(
            ".{name}.dext-tmp-{}-{stamp}-{ordinal}",
            std::process::id()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mode = match expected {
                ExpectedFile::Regular(fingerprint) => fingerprint.mode.unwrap_or(0o600),
                ExpectedFile::Missing => 0o666,
            };
            options.mode(mode);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "creating mutation temp in {}: {error}",
                    parent.display()
                ));
            }
        }
    }
    Err(format!(
        "could not allocate a unique mutation temp in {}",
        parent.display()
    ))
}

fn apply_preserved_permissions(
    file: &std::fs::File,
    expected: &ExpectedFile,
) -> Result<(), String> {
    #[cfg(unix)]
    if let ExpectedFile::Regular(fingerprint) = expected {
        use std::os::unix::fs::PermissionsExt;
        if let Some(mode) = fingerprint.mode {
            file.set_permissions(std::fs::Permissions::from_mode(mode & 0o777))
                .map_err(|error| format!("preserving destination permissions: {error}"))?;
        }
    }
    #[cfg(not(unix))]
    let _ = (file, expected);
    Ok(())
}

#[cfg(windows)]
fn replace_file_atomically(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let from = from
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let to = to
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file_atomically(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

fn sync_parent_best_effort(parent: &Path) {
    #[cfg(unix)]
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    #[cfg(not(unix))]
    let _ = parent;
}

fn render_match_locations(
    path: &Path,
    root: &Path,
    content: &str,
    old: &str,
    count: usize,
) -> String {
    let display = path.strip_prefix(root).unwrap_or(path).display();
    let mut output = format!(
        "old_string appears {count} times in {} — must be unique\n",
        path.display()
    );
    for (index, (byte_index, _)) in content
        .match_indices(old)
        .take(MATCH_DISPLAY_LIMIT)
        .enumerate()
    {
        let prefix = &content[..byte_index];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let line_start = prefix.rfind('\n').map_or(0, |position| position + 1);
        let column = content[line_start..byte_index].chars().count() + 1;
        let text = content[byte_index..]
            .split_once('\n')
            .map_or(&content[byte_index..], |(line, _)| line);
        output.push_str(&format!(
            "match {}: {display}:{line}:{column}\n> {line}\t{text}\n",
            index + 1
        ));
    }
    output
}

fn compute_preview(path: PathBuf, before: &str, after: &str, is_new_file: bool) -> MutationPreview {
    if before == after && !is_new_file {
        return MutationPreview {
            path,
            is_new_file: false,
            added: 0,
            removed: 0,
            diff: "(no changes)".to_string(),
            truncated: false,
        };
    }
    let diff = compute_simple_diff(before, after);
    let added = diff
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .count();
    let removed = diff
        .lines()
        .filter(|line| line.starts_with('-') && !line.starts_with("---"))
        .count();
    let truncated = diff.len() > PREVIEW_DIFF_CAP;
    MutationPreview {
        path,
        is_new_file,
        added,
        removed,
        diff: cap_string(&diff, PREVIEW_DIFF_CAP),
        truncated,
    }
}

fn compute_simple_diff(before: &str, after: &str) -> String {
    let mut result = String::new();
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let max = before_lines.len().max(after_lines.len());
    let mut in_hunk = false;
    for index in 0..max {
        let before_line = before_lines.get(index);
        let after_line = after_lines.get(index);
        if before_line != after_line {
            if !in_hunk {
                if let Some(line) = index
                    .checked_sub(1)
                    .and_then(|previous| before_lines.get(previous))
                {
                    result.push_str(&format!(" {line}\n"));
                }
                in_hunk = true;
            }
            if let Some(line) = before_line {
                result.push_str(&format!("-{line}\n"));
            }
            if let Some(line) = after_line {
                result.push_str(&format!("+{line}\n"));
            }
        } else if in_hunk {
            if let Some(line) = before_line.or(after_line) {
                result.push_str(&format!(" {line}\n"));
            }
            in_hunk = false;
        }
    }
    result
}

fn cap_string(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut result = String::with_capacity(max + 32);
    for line in value.lines() {
        if result.len() + line.len() + 1 > max {
            result.push_str("\n... (preview truncated)");
            break;
        }
        result.push_str(line);
        result.push('\n');
    }
    result
}

#[cfg(test)]
pub(crate) fn preview_write_file(
    root: &Path,
    path: &str,
    content: &str,
) -> Result<MutationPreview, String> {
    Ok(prepare_write_file(root, path, content)?.preview())
}
