use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const FINGERPRINT_PATH_MAX: usize = 4096;
const FINGERPRINT_TOTAL_BYTES_MAX: u64 = 128 * 1024 * 1024;
const FINGERPRINT_EXTERNAL_INPUT_BYTES_MAX: u64 = 4 * 1024 * 1024;
const DEXT_MANIFEST_MAX_BYTES: u64 = 1024 * 1024;

pub(crate) const EVIDENCE_VERSION: u32 = 2;
pub(crate) const EVIDENCE_MAX_AGE_SECS: u64 = 6 * 60 * 60;

pub(crate) const REQUIRED_GATES: &[&str] = &[
    "fmt",
    "clippy",
    "audit",
    "deny",
    "tui",
    "build-release",
    "test-release",
];

#[derive(Clone, Debug)]
pub(crate) struct CommandSpec {
    pub(crate) key: String,
    pub(crate) scopes: Vec<&'static str>,
    working_directory: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub(crate) struct CodeLoopState {
    pub(crate) mutation_sequence: u64,
    focused_verifications: u32,
    full_verifications: u32,
    recent_actions: Vec<String>,
}

impl CodeLoopState {
    pub(crate) fn guard(&self, next: &str) -> Option<String> {
        let start = self
            .recent_actions
            .iter()
            .rposition(|action| action == "R")
            .map_or(0, |index| index + 1);
        let mut actions = self.recent_actions[start..]
            .iter()
            .rev()
            .take(5)
            .rev()
            .cloned()
            .collect::<Vec<_>>();
        actions.push(next.to_string());
        let alternating = actions.len() == 6
            && actions
                .windows(2)
                .all(|pair| (pair[0] == "M") != (pair[1] == "M"));
        alternating.then(|| {
            "coding loop guard: three edit/verification cycles repeated without a strategy checkpoint. PIVOT REQUIRED — inspect the focused diff, consolidate the next edit batch, and change approach before running more mutations or verification.".to_string()
        })
    }

    pub(crate) fn note_mutation(&mut self) {
        self.mutation_sequence = self.mutation_sequence.saturating_add(1);
        self.note("M");
    }

    pub(crate) fn note_verification(&mut self, scope: &str) {
        if scope == "full" {
            self.full_verifications = self.full_verifications.saturating_add(1);
            self.note("F");
        } else {
            self.focused_verifications = self.focused_verifications.saturating_add(1);
            self.note("V");
        }
    }

    pub(crate) fn note_review(&mut self) {
        self.note("R");
    }

    fn note(&mut self, action: &str) {
        self.recent_actions.push(action.to_string());
        if self.recent_actions.len() > 16 {
            self.recent_actions.remove(0);
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub(crate) struct VerificationRecord {
    pub(crate) evidence_version: u32,
    pub(crate) gate_eligible: bool,
    #[serde(skip, default)]
    pub(crate) reusable: bool,
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) command_key: String,
    pub(crate) workspace_fingerprint: String,
    #[serde(skip, default = "empty_runtime_id")]
    pub(crate) runtime_id: String,
    pub(crate) completed_at_secs: u64,
    pub(crate) mutation_sequence: u64,
    pub(crate) scope: String,
    pub(crate) status: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) duration_ms: u64,
    pub(crate) artifact: Option<String>,
    pub(crate) validates: Vec<String>,
}

fn empty_runtime_id() -> String {
    String::new()
}

impl Default for VerificationRecord {
    fn default() -> Self {
        Self {
            evidence_version: 0,
            gate_eligible: false,
            reusable: true,
            name: String::new(),
            command: String::new(),
            command_key: String::new(),
            workspace_fingerprint: String::new(),
            runtime_id: runtime_id().to_string(),
            completed_at_secs: crate::unix_timestamp_secs(),
            mutation_sequence: 0,
            scope: String::new(),
            status: String::new(),
            exit_code: None,
            duration_ms: 0,
            artifact: None,
            validates: Vec::new(),
        }
    }
}

impl CommandSpec {
    pub(crate) fn scope_label(&self) -> String {
        self.scopes.join("+")
    }

    pub(crate) fn is_install(&self) -> bool {
        self.scopes.contains(&"install")
    }

    pub(crate) fn effective_directory(&self, root: &Path) -> Option<PathBuf> {
        let candidate = match self.working_directory.as_deref() {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(path) => root.join(path),
            None => root.to_path_buf(),
        };
        let canonical = std::fs::canonicalize(candidate).ok()?;
        canonical.is_dir().then_some(canonical)
    }
}

pub(crate) fn classify(command: &str) -> Option<CommandSpec> {
    if command
        .chars()
        .any(|ch| matches!(ch, ';' | '|' | '<' | '>' | '`' | '$' | '\\' | '\'' | '"'))
    {
        return None;
    }
    let lines = command
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>();
    let multiline = lines.len() > 1;
    if multiline && !valid_multiline_setup(lines[0]) {
        return None;
    }

    let mut segments = Vec::new();
    for line in lines {
        for segment in line.split("&&") {
            let segment = segment.trim();
            if segment.is_empty() || segment.contains('&') {
                return None;
            }
            segments.push(segment);
        }
    }

    let mut index = 0usize;
    let mut working_directory = None;
    if let Some(segment) = segments.first()
        && let Some(path) = cd_path(segment)
    {
        working_directory = Some(PathBuf::from(path));
        index += 1;
    }
    let fail_fast = segments.get(index).copied() == Some("set -euo pipefail");
    if fail_fast {
        index += 1;
    } else if multiline {
        return None;
    }

    let mut commands = Vec::new();
    let mut scopes = Vec::new();
    for segment in &segments[index..] {
        let words = segment.split_whitespace().collect::<Vec<_>>();
        let scope = verification_scope(&words)?;
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
        commands.push(words.join(" "));
    }
    if commands.is_empty() {
        return None;
    }
    if scopes.contains(&"install") && commands.len() != 1 {
        return None;
    }
    let key = working_directory
        .as_ref()
        .map(|path| format!("cd {} && ", path.display()))
        .unwrap_or_default()
        + &commands.join(" && ");
    Some(CommandSpec {
        key,
        scopes,
        working_directory,
    })
}

fn valid_multiline_setup(first_line: &str) -> bool {
    let setup = first_line.split("&&").map(str::trim).collect::<Vec<_>>();
    match setup.as_slice() {
        ["set -euo pipefail"] => true,
        [cd, "set -euo pipefail"] => cd_path(cd).is_some(),
        _ => false,
    }
}

fn cd_path(segment: &str) -> Option<&str> {
    let words = segment.split_whitespace().collect::<Vec<_>>();
    let ["cd", path] = words.as_slice() else {
        return None;
    };
    (!path.is_empty()
        && *path != "-"
        && !path
            .chars()
            .any(|ch| matches!(ch, '~' | '*' | '?' | '[' | ']' | '{' | '}' | '!')))
    .then_some(*path)
}

fn verification_scope(words: &[&str]) -> Option<&'static str> {
    if words.iter().any(|word| {
        matches!(
            *word,
            "-h" | "--help" | "-V" | "--version" | "--list" | "--no-run"
        )
    }) {
        return None;
    }
    match words {
        ["cargo", "fmt", "--all", "--", "--check"] => Some("fmt"),
        [
            "cargo",
            "clippy",
            "-p",
            "dext",
            "--all-targets",
            "--all-features",
            "--locked",
            "--no-deps",
            "--",
            "-D",
            "warnings",
        ] => Some("clippy"),
        ["cargo", "audit", "--deny", "warnings"] => Some("audit"),
        ["cargo", "deny", "check", "licenses"] => Some("deny"),
        [
            "cargo",
            "test",
            "--release",
            "--locked",
            "--bin",
            "dext",
            "tui::tests::",
        ] => Some("tui"),
        ["cargo", "build", "--release", "--locked"] => Some("build-release"),
        ["cargo", "test", "--release", "--locked"] => Some("test-release"),
        ["cargo", "install", "--path", ".", "--force", "--locked"] => Some("install"),
        ["cargo", "test", ..] | ["cargo", "nextest", ..] => Some("focused"),
        ["cargo", "check", ..] | ["cargo", "clippy", ..] | ["cargo", "build", ..] => Some("check"),
        ["cargo", "audit", ..] | ["cargo", "deny", "check", ..] => Some("focused"),
        ["npm" | "pnpm" | "yarn", "test", ..]
        | ["pytest", ..]
        | ["go" | "mix", "test", ..]
        | ["zig", "build", "test", ..]
        | ["swift" | "dotnet", "test", ..]
        | ["mvn" | "gradle", "test", ..] => Some("focused"),
        _ => None,
    }
}

pub(crate) fn evidence_status(
    command_succeeded: bool,
    before: Option<&str>,
    after: Option<&str>,
) -> &'static str {
    if !command_succeeded {
        "failed"
    } else {
        match (before, after) {
            (Some(before), Some(after)) if before == after => "passed",
            (Some(_), Some(_)) => "workspace-changed",
            _ => "workspace-unavailable",
        }
    }
}

pub(crate) fn evidence_is_fresh(completed_at_secs: u64, now_secs: u64) -> bool {
    completed_at_secs <= now_secs
        && now_secs.saturating_sub(completed_at_secs) <= EVIDENCE_MAX_AGE_SECS
}

pub(crate) fn runtime_id() -> &'static str {
    "current-process"
}

#[cfg(test)]
pub(crate) fn workspace_fingerprint(root: &Path) -> Option<String> {
    workspace_fingerprint_for_tool(root, &[], "danger-full-access")
}

pub(crate) fn workspace_fingerprint_for_tool(
    root: &Path,
    extra_env: &[(String, String)],
    execution_profile: &str,
) -> Option<String> {
    let command_root = std::fs::canonicalize(root).ok()?;
    if !command_root.is_dir() {
        return None;
    }
    let top = git_toplevel(&command_root)?;
    let head = crate::run_internal_git_command(&top, &["rev-parse", "--verify", "HEAD"]).ok()?;
    let raw = crate::run_internal_git_command(&top, &["diff", "--raw", "-z", "HEAD", "--"]).ok()?;
    let tracked = crate::run_internal_git_command(&top, &["ls-files", "-z"]).ok()?;
    let index = crate::run_internal_git_command(&top, &["ls-files", "--stage", "-z"]).ok()?;
    let untracked = crate::run_internal_git_command(
        &top,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .ok()?;
    let ignored_build_inputs = if is_dext_package(&top) {
        crate::run_internal_git_command(
            &top,
            &[
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "-z",
                "--",
                ".",
                ":(exclude)target/**",
                ":(exclude).dext/**",
                ":(exclude).auto/**",
            ],
        )
        .ok()
    } else {
        None
    };
    if !head.success()
        || !raw.success()
        || !tracked.success()
        || !index.success()
        || !untracked.success()
        || ignored_build_inputs
            .as_ref()
            .is_some_and(|output| !output.success())
    {
        return None;
    }

    let mut paths = nul_paths(&tracked.stdout)?;
    paths.extend(nul_paths(&untracked.stdout)?);
    if let Some(ignored) = ignored_build_inputs {
        paths.extend(nul_paths(&ignored.stdout)?.into_iter().filter(|path| {
            !["target", ".dext", ".auto"]
                .iter()
                .any(|root| path.starts_with(root))
        }));
    }
    paths.sort();
    paths.dedup();
    if paths.len() > FINGERPRINT_PATH_MAX {
        return None;
    }

    let mut total_bytes = 0u64;
    let mut hash = Sha256::new();
    hash.update(b"dext-workspace-v3\0");
    hash_path_identity(&mut hash, &top);
    hash.update([0]);
    hash_path_identity(&mut hash, &command_root);
    hash.update([0]);
    hash.update(execution_profile.as_bytes());
    hash.update([0]);
    hash.update(head.stdout);
    hash.update(raw.stdout);
    hash.update(index.stdout);
    hash_environment(&mut hash, extra_env)?;
    hash_external_build_inputs(&mut hash, &command_root)?;
    for relative in paths {
        if !safe_relative(&relative) {
            return None;
        }
        hash.update(relative.to_string_lossy().as_bytes());
        hash.update([0]);
        let path = top.join(&relative);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                hash.update(b"missing\0");
                continue;
            }
            Err(_) => return None,
        };
        hash.update(metadata.len().to_le_bytes());
        hash_file_mode(&mut hash, &metadata);
        total_bytes = total_bytes.checked_add(metadata.len())?;
        if total_bytes > FINGERPRINT_TOTAL_BYTES_MAX {
            return None;
        }
        if metadata.file_type().is_symlink() {
            hash.update(b"symlink\0");
            hash.update(std::fs::read_link(&path).ok()?.to_string_lossy().as_bytes());
            let target = std::fs::metadata(&path).ok()?;
            if !target.is_file() {
                return None;
            }
            hash.update(target.len().to_le_bytes());
            hash_file_mode(&mut hash, &target);
            total_bytes = total_bytes.checked_add(target.len())?;
            if total_bytes > FINGERPRINT_TOTAL_BYTES_MAX {
                return None;
            }
            hash_file(&mut hash, &path)?;
        } else if metadata.is_file() {
            hash.update(b"file\0");
            hash_file(&mut hash, &path)?;
        } else {
            return None;
        }
    }
    let digest = hash.finalize();
    Some(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn hash_file(hash: &mut Sha256, path: &Path) -> Option<()> {
    let mut file = File::open(path).ok()?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            return Some(());
        }
        hash.update(&buffer[..read]);
    }
}

fn hash_file_mode(hash: &mut Sha256, metadata: &std::fs::Metadata) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        hash.update(metadata.permissions().mode().to_le_bytes());
    }
    #[cfg(not(unix))]
    hash.update([u8::from(metadata.permissions().readonly())]);
}

fn hash_environment(hash: &mut Sha256, extra_env: &[(String, String)]) -> Option<()> {
    if crate::tool_children_inherit_credentials() {
        return None;
    }
    let mut environment = std::env::vars_os().collect::<std::collections::BTreeMap<_, _>>();
    for key in environment.keys().cloned().collect::<Vec<_>>() {
        let key_text = key.to_string_lossy();
        if crate::INTERNAL_STARTUP_ENV_VARS
            .iter()
            .any(|blocked| key_text.eq_ignore_ascii_case(blocked))
            || key_text.starts_with("BASH_FUNC_") && key_text.ends_with("%%")
            || crate::tool_credential_env_key(&key_text)
        {
            environment.remove(&key);
        }
    }
    for (key, value) in extra_env {
        if !crate::tool_credential_env_key(key) {
            environment.insert(key.into(), value.into());
        }
    }
    for (key, value) in environment {
        hash_path_identity(hash, Path::new(&key));
        hash.update([0]);
        hash_path_identity(hash, Path::new(&value));
        hash.update([0]);
    }
    Some(())
}

fn hash_external_build_inputs(hash: &mut Sha256, command_root: &Path) -> Option<()> {
    let mut paths = Vec::new();
    for ancestor in command_root.ancestors() {
        paths.push(ancestor.join(".cargo/config"));
        paths.push(ancestor.join(".cargo/config.toml"));
        paths.push(ancestor.join("rust-toolchain"));
        paths.push(ancestor.join("rust-toolchain.toml"));
    }
    if let Some(mut cargo_home) = std::env::var_os("CARGO_HOME").map(PathBuf::from) {
        if cargo_home.is_relative() {
            cargo_home = command_root.join(cargo_home);
        }
        paths.push(cargo_home.join("config"));
        paths.push(cargo_home.join("config.toml"));
    } else {
        let cargo_home = crate::session::user_home_dir().join(".cargo");
        paths.push(cargo_home.join("config"));
        paths.push(cargo_home.join("config.toml"));
    }
    paths.sort();
    paths.dedup();

    let mut total_bytes = 0u64;
    for path in paths {
        hash_path_identity(hash, &path);
        hash.update([0]);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                hash.update(b"missing\0");
                continue;
            }
            Err(_) => return None,
        };
        if metadata.file_type().is_symlink() {
            hash.update(b"symlink\0");
            hash.update(std::fs::read_link(&path).ok()?.to_string_lossy().as_bytes());
        } else if !metadata.is_file() {
            return None;
        }
        let target = std::fs::metadata(&path).ok()?;
        if !target.is_file() {
            return None;
        }
        total_bytes = total_bytes.checked_add(target.len())?;
        if total_bytes > FINGERPRINT_EXTERNAL_INPUT_BYTES_MAX {
            return None;
        }
        hash.update(target.len().to_le_bytes());
        hash_file_mode(hash, &target);
        hash_file(hash, &path)?;
    }
    Some(())
}

fn hash_path_identity(hash: &mut Sha256, path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        hash.update(path.as_os_str().as_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        for unit in path.as_os_str().encode_wide() {
            hash.update(unit.to_le_bytes());
        }
    }
    #[cfg(not(any(unix, windows)))]
    hash.update(path.to_string_lossy().as_bytes());
}

pub(crate) fn is_git_toplevel(root: &Path) -> bool {
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    git_toplevel(&root).is_some_and(|top| top == root)
}

pub(crate) fn dext_package_status(root: &Path) -> Option<bool> {
    let root = std::fs::canonicalize(root).ok()?;
    let manifest_path = root.join("Cargo.toml");
    let metadata = std::fs::metadata(&manifest_path).ok()?;
    if !metadata.is_file() || metadata.len() > DEXT_MANIFEST_MAX_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(manifest_path).ok()?;
    text.parse::<toml_edit::Document<String>>()
        .ok()
        .and_then(|manifest| {
            manifest["package"]["name"]
                .as_str()
                .map(|name| name == "dext")
        })
}

pub(crate) fn is_dext_package(root: &Path) -> bool {
    dext_package_status(root) == Some(true)
}

pub(crate) fn install_working_directory(command: &str, root: &Path) -> Option<PathBuf> {
    if command
        .chars()
        .any(|ch| matches!(ch, ';' | '|' | '<' | '>' | '`' | '$' | '\\' | '\'' | '"'))
    {
        return None;
    }
    let mut segments = Vec::new();
    for line in command
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        for segment in line.split("&&") {
            let segment = segment.trim();
            if segment.is_empty() || segment.contains('&') {
                return None;
            }
            segments.push(segment);
        }
    }
    if crate::tool_policy::command_has_wrapper_directory_change(command) {
        return None;
    }
    let mut index = 0usize;
    let working_directory = if let Some(path) = segments.first().and_then(|line| cd_path(line)) {
        index += 1;
        Some(path)
    } else {
        None
    };
    if segments.get(index).copied() == Some("set -euo pipefail") {
        index += 1;
    }
    if segments[index..].iter().any(|segment| {
        segment
            .split_whitespace()
            .next()
            .is_some_and(|command| matches!(command, "cd" | "pushd" | "popd" | "source" | "."))
    }) {
        return None;
    }
    let candidate = working_directory.map_or_else(
        || root.to_path_buf(),
        |path| {
            let path = Path::new(path);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            }
        },
    );
    let canonical = std::fs::canonicalize(candidate).ok()?;
    canonical.is_dir().then_some(canonical)
}

pub(crate) fn is_dext_checkout(root: &Path) -> bool {
    git_toplevel(root).is_some_and(|top| is_dext_package(&top))
}

fn git_toplevel(root: &Path) -> Option<PathBuf> {
    let top = crate::run_internal_git_command(root, &["rev-parse", "--show-toplevel"]).ok()?;
    if !top.success() {
        return None;
    }
    let path = PathBuf::from(std::str::from_utf8(&top.stdout).ok()?.trim());
    std::fs::canonicalize(path).ok()
}

fn nul_paths(bytes: &[u8]) -> Option<Vec<PathBuf>> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| std::str::from_utf8(path).ok().map(PathBuf::from))
        .collect()
}

fn safe_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::process::Command;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn classifies_only_explicit_verification_commands() {
        let spec = classify(
            "set -euo pipefail\ncargo fmt --all -- --check\ncargo clippy -p dext --all-targets --all-features --locked --no-deps -- -D warnings\ncargo audit --deny warnings",
        )
        .unwrap();
        assert_eq!(spec.scopes, ["fmt", "clippy", "audit"]);
        assert!(
            classify("cargo install --path . --force --locked")
                .unwrap()
                .is_install()
        );
        assert!(classify("cargo fmt --all").is_none());
        for command in [
            "cargo test\ncargo audit --deny warnings",
            "cd /tmp\ncargo test",
            "echo cargo clippy",
            "cargo clippy || true",
            "cargo test & echo done",
            "cargo test; touch src/main.rs",
            "cargo test && touch src/main.rs",
            "./verify-release.sh",
            "printf 'cargo audit'",
            "cd - && cargo test",
            "cd ~/repo && cargo test",
            "cd repo && cd nested && cargo test",
        ] {
            assert!(classify(command).is_none(), "{command}");
        }
    }

    #[test]
    fn required_gate_scopes_require_canonical_commands() {
        let canonical = [
            ("cargo fmt --all -- --check", "fmt"),
            (
                "cargo clippy -p dext --all-targets --all-features --locked --no-deps -- -D warnings",
                "clippy",
            ),
            ("cargo audit --deny warnings", "audit"),
            ("cargo deny check licenses", "deny"),
            (
                "cargo test --release --locked --bin dext tui::tests::",
                "tui",
            ),
            ("cargo build --release --locked", "build-release"),
            ("cargo test --release --locked", "test-release"),
        ];
        for (command, scope) in canonical {
            assert_eq!(classify(command).unwrap().scopes, [scope], "{command}");
        }

        let lookalikes = [
            ("cargo fmt --all --check", "fmt"),
            (
                "cargo clippy -p dext --all-targets --all-features --no-deps -- -D warnings",
                "clippy",
            ),
            ("cargo audit --help", "audit"),
            ("cargo deny check", "deny"),
            (
                "cargo test --release --locked --bin dext tui::tests::one_test",
                "tui",
            ),
            ("cargo build --release", "build-release"),
            (
                "cargo test --release --locked nonexistent_filter",
                "test-release",
            ),
        ];
        for (command, forbidden_scope) in lookalikes {
            assert!(
                classify(command).is_none_or(|spec| !spec.scopes.contains(&forbidden_scope)),
                "{command}"
            );
        }
        for command in [
            "cargo test --help",
            "cargo test --version",
            "cargo test -- --list",
            "cargo test --no-run",
            "cargo check --help",
            "cargo build -V",
        ] {
            assert!(classify(command).is_none(), "{command}");
        }
        assert!(classify("cargo install --path . --force").is_none());
        assert!(
            classify("cargo install --path . --force --locked && cargo test --release --locked")
                .is_none()
        );
    }

    #[test]
    fn verification_keys_preserve_case_and_bind_the_effective_directory() {
        let _guard = crate::test_env_lock();
        let root = std::env::temp_dir().join(format!(
            "dext-verification-directory-{}-{}",
            std::process::id(),
            crate::unix_timestamp_secs()
        ));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        for (path, content) in [(&first, "first\n"), (&second, "second\n")] {
            git(path, &["init", "-q"]);
            git(path, &["config", "user.email", "test@example.invalid"]);
            git(path, &["config", "user.name", "Test"]);
            std::fs::write(path.join("tracked.txt"), content).unwrap();
            git(path, &["add", "tracked.txt"]);
            git(path, &["commit", "-q", "-m", "base"]);
        }

        let upper = classify("cd first && cargo test ParserCase").unwrap();
        let lower = classify("cd first && cargo test parsercase").unwrap();
        let other = classify("cd second && cargo test ParserCase").unwrap();
        assert_ne!(upper.key, lower.key);
        assert_ne!(upper.key, other.key);
        let upper_root = upper.effective_directory(&root).unwrap();
        let other_root = other.effective_directory(&root).unwrap();
        assert_eq!(upper_root, std::fs::canonicalize(&first).unwrap());
        assert_eq!(other_root, std::fs::canonicalize(&second).unwrap());
        assert_eq!(
            workspace_fingerprint(&upper_root),
            workspace_fingerprint(&first)
        );
        assert_ne!(
            workspace_fingerprint(&upper_root),
            workspace_fingerprint(&other_root)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn verification_evidence_requires_an_unchanged_workspace() {
        assert_eq!(evidence_status(true, Some("a"), Some("a")), "passed");
        assert_eq!(
            evidence_status(true, Some("a"), Some("b")),
            "workspace-changed"
        );
        assert_eq!(
            evidence_status(true, Some("a"), None),
            "workspace-unavailable"
        );
        assert_eq!(evidence_status(false, Some("a"), Some("a")), "failed");
    }

    #[test]
    fn workspace_fingerprint_changes_with_tracked_and_untracked_content() {
        let _guard = crate::test_env_lock();
        let root = std::env::temp_dir().join(format!(
            "dext-verification-fingerprint-{}-{}",
            std::process::id(),
            crate::unix_timestamp_secs()
        ));
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "test@example.invalid"]);
        git(&root, &["config", "user.name", "Test"]);
        std::fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-q", "-m", "base"]);
        let clean = workspace_fingerprint(&root).unwrap();
        std::fs::write(root.join("tracked.txt"), "changed\n").unwrap();
        let tracked = workspace_fingerprint(&root).unwrap();
        assert_ne!(clean, tracked);
        std::fs::write(root.join("untracked.txt"), "new\n").unwrap();
        let untracked = workspace_fingerprint(&root).unwrap();
        assert_ne!(tracked, untracked);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_fingerprint_excludes_ignored_runtime_repositories_but_not_tracked_files() {
        let _guard = crate::test_env_lock();
        let root = std::env::temp_dir().join(format!(
            "dext-verification-runtime-repos-{}-{}",
            std::process::id(),
            crate::unix_timestamp_secs()
        ));
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "test@example.invalid"]);
        git(&root, &["config", "user.name", "Test"]);
        std::fs::write(root.join(".gitignore"), "/target/\n/.dext/\n/.auto/\n").unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"dext\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        git(&root, &["add", ".gitignore", "Cargo.toml"]);
        git(&root, &["commit", "-q", "-m", "base"]);
        let clean = workspace_fingerprint(&root).unwrap();
        for directory in ["target", ".dext", ".auto"] {
            let nested = root.join(directory).join("nested");
            std::fs::create_dir_all(&nested).unwrap();
            git(&nested, &["init", "-q"]);
            std::fs::write(nested.join("scratch.txt"), "runtime\n").unwrap();
        }
        assert_eq!(workspace_fingerprint(&root).unwrap(), clean);
        std::fs::write(root.join(".dext/tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "-f", ".dext/tracked.txt"]);
        git(&root, &["commit", "-q", "-m", "tracked runtime path"]);
        let tracked = workspace_fingerprint(&root).unwrap();
        std::fs::write(root.join(".dext/tracked.txt"), "changed\n").unwrap();
        assert_ne!(workspace_fingerprint(&root).unwrap(), tracked);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_fingerprint_hashes_tracked_files_hidden_by_index_flags() {
        let _guard = crate::test_env_lock();
        let root = std::env::temp_dir().join(format!(
            "dext-verification-index-flags-{}-{}",
            std::process::id(),
            crate::unix_timestamp_secs()
        ));
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "test@example.invalid"]);
        git(&root, &["config", "user.name", "Test"]);
        std::fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["add", "tracked.txt"]);
        git(&root, &["commit", "-q", "-m", "base"]);
        let clean = workspace_fingerprint(&root).unwrap();

        git(
            &root,
            &["update-index", "--assume-unchanged", "tracked.txt"],
        );
        std::fs::write(root.join("tracked.txt"), "assume changed\n").unwrap();
        assert_ne!(clean, workspace_fingerprint(&root).unwrap());
        git(
            &root,
            &["update-index", "--no-assume-unchanged", "tracked.txt"],
        );

        std::fs::write(root.join("tracked.txt"), "base\n").unwrap();
        git(&root, &["update-index", "--skip-worktree", "tracked.txt"]);
        std::fs::write(root.join("tracked.txt"), "skip changed\n").unwrap();
        assert_ne!(clean, workspace_fingerprint(&root).unwrap());
        git(
            &root,
            &["update-index", "--no-skip-worktree", "tracked.txt"],
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn workspace_fingerprint_changes_with_hidden_cargo_config_and_executable_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let _guard = crate::test_env_lock();

        let root = std::env::temp_dir().join(format!(
            "dext-verification-hidden-inputs-{}-{}",
            std::process::id(),
            crate::unix_timestamp_secs()
        ));
        std::fs::create_dir_all(root.join(".cargo")).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "test@example.invalid"]);
        git(&root, &["config", "user.name", "Test"]);
        git(&root, &["config", "core.filemode", "false"]);
        std::fs::write(
            root.join(".gitignore"),
            ".cargo/config.toml\nignored-build-input.txt\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"dext\"\nversion = \"0.0.0\"\n",
        )
        .unwrap();
        std::fs::write(root.join("tracked.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        git(&root, &["add", ".gitignore", "Cargo.toml", "tracked.sh"]);
        git(&root, &["commit", "-q", "-m", "base"]);
        let clean = workspace_fingerprint(&root).unwrap();

        let mut permissions = std::fs::metadata(root.join("tracked.sh"))
            .unwrap()
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(root.join("tracked.sh"), permissions).unwrap();
        let executable = workspace_fingerprint(&root).unwrap();
        assert_ne!(clean, executable);

        std::fs::write(
            root.join(".cargo/config.toml"),
            "[build]\nrustflags = [\"--cfg\", \"hidden_config\"]\n",
        )
        .unwrap();
        let hidden_config = workspace_fingerprint(&root).unwrap();
        assert_ne!(executable, hidden_config);

        std::fs::write(root.join("ignored-build-input.txt"), "first\n").unwrap();
        let ignored = workspace_fingerprint(&root).unwrap();
        assert_ne!(hidden_config, ignored);

        let first_env = workspace_fingerprint_for_tool(
            &root,
            &[("DEXT_FINGERPRINT_TEST".to_string(), "first".to_string())],
            "danger-full-access",
        )
        .unwrap();
        let second_env = workspace_fingerprint_for_tool(
            &root,
            &[("DEXT_FINGERPRINT_TEST".to_string(), "second".to_string())],
            "danger-full-access",
        )
        .unwrap();
        let confined = workspace_fingerprint_for_tool(
            &root,
            &[("DEXT_FINGERPRINT_TEST".to_string(), "second".to_string())],
            "workspace-write",
        )
        .unwrap();
        assert_ne!(first_env, second_env);
        assert_ne!(second_env, confined);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_fingerprints_are_bound_to_the_checkout_path() {
        let _guard = crate::test_env_lock();
        let parent = std::env::temp_dir().join(format!(
            "dext-verification-clones-{}-{}",
            std::process::id(),
            crate::unix_timestamp_secs()
        ));
        let first = parent.join("first");
        let second = parent.join("second");
        std::fs::create_dir_all(&first).unwrap();
        git(&first, &["init", "-q"]);
        git(&first, &["config", "user.email", "test@example.invalid"]);
        git(&first, &["config", "user.name", "Test"]);
        std::fs::write(first.join("tracked.txt"), "same\n").unwrap();
        git(&first, &["add", "tracked.txt"]);
        git(&first, &["commit", "-q", "-m", "base"]);
        let output = Command::new("git")
            .current_dir(&parent)
            .args([
                "clone",
                "-q",
                first.to_str().unwrap(),
                second.to_str().unwrap(),
            ])
            .output()
            .expect("clone fixture repository");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            crate::run_internal_git_command(&first, &["rev-parse", "HEAD"])
                .unwrap()
                .stdout,
            crate::run_internal_git_command(&second, &["rev-parse", "HEAD"])
                .unwrap()
                .stdout
        );
        assert_ne!(
            workspace_fingerprint(&first),
            workspace_fingerprint(&second)
        );
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn dext_package_and_git_toplevel_detection_are_independent() {
        let root = std::env::temp_dir().join(format!(
            "dext-verification-checkout-{}-{}",
            std::process::id(),
            crate::unix_timestamp_secs()
        ));
        std::fs::create_dir_all(root.join("nested")).unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"dext\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert!(is_git_toplevel(&root));
        assert!(!is_git_toplevel(&root.join("nested")));
        assert!(is_dext_package(&root));
        assert!(is_dext_checkout(&root));
        assert!(!is_dext_package(&root.join("nested")));
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"dex\\u0074\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        assert!(is_dext_package(&root));
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"other\"\n").unwrap();
        assert!(!is_dext_package(&root));
        std::fs::remove_dir_all(root).unwrap();
    }
}
