use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

const FINGERPRINT_PATH_MAX: usize = 4096;
const FINGERPRINT_TOTAL_BYTES_MAX: u64 = 128 * 1024 * 1024;

pub(crate) const REQUIRED_GATES: &[&str] = &[
    "fmt",
    "clippy",
    "audit",
    "deny",
    "ratatui",
    "build-release",
    "test-release",
];

#[derive(Clone, Debug)]
pub(crate) struct CommandSpec {
    pub(crate) key: String,
    pub(crate) scopes: Vec<&'static str>,
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

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub(crate) struct VerificationRecord {
    pub(crate) name: String,
    pub(crate) command: String,
    pub(crate) command_key: String,
    pub(crate) workspace_fingerprint: String,
    pub(crate) mutation_sequence: u64,
    pub(crate) scope: String,
    pub(crate) status: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) duration_ms: u64,
    pub(crate) artifact: Option<String>,
    pub(crate) validates: Vec<String>,
}

impl CommandSpec {
    pub(crate) fn scope_label(&self) -> String {
        self.scopes.join("+")
    }

    pub(crate) fn is_install(&self) -> bool {
        self.scopes.contains(&"install")
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
    if lines.len() > 1 {
        let setup = lines[0].split("&&").map(str::trim).collect::<Vec<_>>();
        if setup.last().copied() != Some("set -euo pipefail")
            || setup[..setup.len().saturating_sub(1)]
                .iter()
                .any(|segment| !is_cd_segment(segment))
        {
            return None;
        }
    }

    let normalized = command.replace('\n', " && ");
    let mut commands = Vec::new();
    let mut scopes = Vec::new();
    for segment in normalized.split("&&") {
        if segment.contains('&') {
            return None;
        }
        let words = segment.split_whitespace().collect::<Vec<_>>();
        if words.is_empty() || words[0] == "#" {
            continue;
        }
        if words[0] == "set" {
            if words.as_slice() != ["set", "-euo", "pipefail"] {
                return None;
            }
            continue;
        }
        if words[0] == "cd" {
            if !is_cd_segment(segment) {
                return None;
            }
            continue;
        }
        let scope = verification_scope(&words)?;
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
        commands.push(words.join(" ").to_ascii_lowercase());
    }
    (!commands.is_empty()).then(|| CommandSpec {
        key: commands.join(" && "),
        scopes,
    })
}

fn is_cd_segment(segment: &str) -> bool {
    let words = segment.split_whitespace().collect::<Vec<_>>();
    words.len() == 2 && words[0] == "cd"
}

fn verification_scope(words: &[&str]) -> Option<&'static str> {
    match words {
        ["cargo", "fmt", rest @ ..] if rest.contains(&"--check") => Some("fmt"),
        ["cargo", "clippy", ..] => Some("clippy"),
        ["cargo", "audit", ..] => Some("audit"),
        ["cargo", "deny", "check", ..] => Some("deny"),
        ["cargo", "test", rest @ ..]
            if rest.windows(2).any(|pair| pair == ["-p", "ratatui-core"]) =>
        {
            Some("ratatui")
        }
        ["cargo", "build", rest @ ..] if rest.contains(&"--release") => Some("build-release"),
        ["cargo", "test", rest @ ..] if rest.contains(&"--release") => Some("test-release"),
        ["cargo", "test", ..] | ["cargo", "nextest", ..] => Some("focused"),
        ["cargo", "check", ..] => Some("check"),
        ["cargo", "install", ..] => Some("install"),
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

pub(crate) fn workspace_fingerprint(root: &Path) -> Option<String> {
    let top = crate::run_internal_git_command(root, &["rev-parse", "--show-toplevel"]).ok()?;
    if !top.success() {
        return None;
    }
    let top = PathBuf::from(std::str::from_utf8(&top.stdout).ok()?.trim());
    let head = crate::run_internal_git_command(&top, &["rev-parse", "--verify", "HEAD"]).ok()?;
    if !head.success() {
        return None;
    }
    let raw = crate::run_internal_git_command(&top, &["diff", "--raw", "-z", "HEAD", "--"]).ok()?;
    let changed =
        crate::run_internal_git_command(&top, &["diff", "--name-only", "-z", "HEAD", "--"]).ok()?;
    let untracked = crate::run_internal_git_command(
        &top,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .ok()?;
    if !raw.success() || !changed.success() || !untracked.success() {
        return None;
    }

    let mut paths = nul_paths(&changed.stdout)?;
    paths.extend(nul_paths(&untracked.stdout)?);
    paths.sort();
    paths.dedup();
    if paths.len() > FINGERPRINT_PATH_MAX {
        return None;
    }

    let mut total_bytes = 0u64;
    let mut hash = Sha256::new();
    hash.update(b"dext-workspace-v1\0");
    hash.update(head.stdout);
    hash.update(raw.stdout);
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
        total_bytes = total_bytes.checked_add(metadata.len())?;
        if total_bytes > FINGERPRINT_TOTAL_BYTES_MAX {
            return None;
        }
        if metadata.file_type().is_symlink() {
            hash.update(b"symlink\0");
            hash.update(std::fs::read_link(path).ok()?.to_string_lossy().as_bytes());
        } else if metadata.is_file() {
            hash.update(b"file\0");
            let mut file = File::open(path).ok()?;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer).ok()?;
                if read == 0 {
                    break;
                }
                hash.update(&buffer[..read]);
            }
        } else {
            hash.update(b"other\0");
        }
    }
    let digest = hash.finalize();
    Some(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(crate) fn is_dext_checkout(root: &Path) -> bool {
    let Ok(top) = crate::run_internal_git_command(root, &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    if !top.success() {
        return false;
    }
    let Ok(top) = std::str::from_utf8(&top.stdout) else {
        return false;
    };
    std::fs::read_to_string(Path::new(top.trim()).join("Cargo.toml"))
        .ok()
        .is_some_and(|text| text.lines().any(|line| line.trim() == "name = \"dext\""))
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
            "set -euo pipefail\ncargo fmt --all -- --check\ncargo clippy -p dext -- -D warnings\ncargo audit --deny warnings",
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
        ] {
            assert!(classify(command).is_none(), "{command}");
        }
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
    fn dext_checkout_detection_resolves_the_git_toplevel_from_subdirectories() {
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
        assert!(is_dext_checkout(&root));
        assert!(is_dext_checkout(&root.join("nested")));
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"other\"\n").unwrap();
        assert!(!is_dext_checkout(&root.join("nested")));
        std::fs::remove_dir_all(root).unwrap();
    }
}
