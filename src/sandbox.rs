// OS-level filesystem sandboxing for tool subprocesses.
//
// This is defense-in-depth that backstops the heuristic command-risk
// classifier and path validation. Unsupported platforms degrade to path
// validation, but once a kernel mechanism is detected, rule/profile setup
// failures stop the child instead of silently running it unconfined.
//
// Enforcement by platform:
//   - Linux:   Landlock LSM (kernel >= 5.13 with the LSM enabled).
//   - macOS:   Seatbelt via `sandbox-exec` wrapping.
//   - Windows: not enforced at the kernel level here (AppContainer would
//              break common Git-Bash installs); writes are still confined by
//              the path-validation layer. Use WSL for kernel-enforced bash.
//
// Profiles:
//   - ReadOnly:        deny writes to the workspace, home, and system; only
//                      scratch (temp dirs) and device nodes (/dev) stay
//                      writable. Reads are allowed outside the user's home;
//                      inside home, only the sandbox root, executable PATH
//                      entries, packs, and toolchain cache roots are visible.
//   - WorkspaceWrite:  additionally permit writes under the sandbox root and
//                      common per-user toolchain cache roots. Shell startup
//                      files and unrelated home-directory content stay
//                      read-only.
//   - DangerFullAccess: no confinement.

use std::path::{Path, PathBuf};

use crate::SandboxProfile;

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn sbpl_path(path: &Path) -> Option<String> {
    let path = path.to_str()?;
    if path.chars().any(char::is_control) {
        return None;
    }
    Some(path.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn sbpl_filter(path: &Path) -> Option<String> {
    let escaped = sbpl_path(path)?;
    let filter = if path.is_dir() { "subpath" } else { "literal" };
    Some(format!("    ({filter} \"{escaped}\")\n"))
}

fn approved_environment_temp_dir() -> Option<PathBuf> {
    let temp = std::fs::canonicalize(std::env::temp_dir()).ok()?;
    #[cfg(target_os = "linux")]
    let parents = ["/tmp", "/var/tmp", "/dev/shm"];
    #[cfg(target_os = "macos")]
    let parents = ["/private/tmp", "/private/var/tmp", "/private/var/folders"];
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let parents: [&str; 0] = [];
    parents
        .into_iter()
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .any(|parent| temp.starts_with(parent))
        .then_some(temp)
}

/// Paths that must stay writable under every profile: scratch space (heredocs,
/// lock files) and the small set of device nodes ordinary commands need.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn scratch_write_roots() -> Vec<PathBuf> {
    let mut roots = approved_environment_temp_dir()
        .into_iter()
        .collect::<Vec<_>>();
    for path in [
        "/tmp",
        "/var/tmp",
        "/dev/shm",
        "/dev/null",
        "/dev/zero",
        "/dev/random",
        "/dev/urandom",
        "/dev/tty",
    ] {
        let path = PathBuf::from(path);
        if path.exists() {
            roots.push(path);
        }
    }
    roots
}

fn sandbox_home_dir() -> PathBuf {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home);
    }
    crate::session::user_home_dir()
}

#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn toolchain_write_roots() -> Vec<PathBuf> {
    let home = sandbox_home_dir();
    [
        ".cache/pip",
        ".cache/uv",
        ".cache/go-build",
        ".cache/node-gyp",
        ".cache/pnpm",
        ".cache/yarn",
        ".cargo/git",
        ".cargo/registry",
        ".npm/_cacache",
        ".npm/_logs",
        ".pnpm-store",
        ".yarn/berry/cache",
        ".yarn/cache",
        ".gradle/caches",
        ".gradle/daemon",
        ".gradle/wrapper",
        ".m2/repository",
        ".ivy2/cache",
        ".ivy2/jars",
        ".ivy2/local",
        ".nuget/packages",
        ".local/share/pnpm/store",
        "go/pkg",
    ]
    .into_iter()
    .map(|path| home.join(path))
    .collect()
}

#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn toolchain_read_roots() -> Vec<PathBuf> {
    let home = sandbox_home_dir();
    let mut roots = toolchain_write_roots();
    roots.extend(
        [
            ".rustup",
            ".pyenv",
            ".nvm",
            ".bun",
            ".local/share/uv",
            ".cargo/bin",
            ".local/bin",
            "bin",
            "go/bin",
        ]
        .into_iter()
        .map(|path| home.join(path)),
    );
    roots.sort();
    roots.dedup();
    roots
}

fn canonical_home_subpath(path: &Path, home: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(home).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    let canonical_home = std::fs::canonicalize(home).ok()?;
    let canonical = std::fs::canonicalize(path).ok()?;
    (canonical == canonical_home.join(relative)).then_some(canonical)
}

fn existing_safe_roots(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let home = sandbox_home_dir();
    let mut roots = paths
        .into_iter()
        .filter_map(|path| {
            if path.starts_with(&home) {
                canonical_home_subpath(&path, &home)
            } else {
                std::fs::canonicalize(path).ok()
            }
        })
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    roots
}

fn canonical_explicit_roots(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut roots = paths
        .into_iter()
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .collect::<Vec<_>>();
    roots.sort();
    roots.dedup();
    roots
}

#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn home_safe_read_roots(root: &Path, extra_read_roots: &[PathBuf]) -> Vec<PathBuf> {
    let home = sandbox_home_dir();
    let state = crate::session::dext_state_dir();
    let mut inferred = toolchain_read_roots();
    inferred.extend([
        state.join("packs"),
        state.join("shelves"),
        home.join(".config/git"),
        home.join(".gitconfig"),
    ]);
    if let Some(path) = std::env::var_os("PATH") {
        inferred.extend(std::env::split_paths(&path).filter(|path| path.starts_with(&home)));
    }
    let mut roots = existing_safe_roots(inferred);
    roots.extend(canonical_explicit_roots(
        std::iter::once(root.to_path_buf()).chain(extra_read_roots.iter().cloned()),
    ));
    roots.sort();
    roots.dedup();
    roots
}

#[cfg(target_os = "linux")]
fn linux_readable_roots(root: &Path, extra_read_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = [
        "/bin",
        "/etc",
        "/lib",
        "/lib64",
        "/nix",
        "/opt",
        "/proc/cpuinfo",
        "/proc/filesystems",
        "/proc/meminfo",
        "/proc/stat",
        "/proc/sys/kernel/osrelease",
        "/proc/sys/vm/overcommit_memory",
        "/sbin",
        "/snap",
        "/sys",
        "/usr",
        "/var/cache",
        "/var/lib/dpkg",
        "/var/lib/rpm",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect::<Vec<_>>();
    if let Some(path) = std::env::var_os("PATH") {
        roots.extend(std::env::split_paths(&path));
    }
    roots.extend(scratch_write_roots());
    roots.extend(home_safe_read_roots(root, extra_read_roots));
    existing_safe_roots(roots)
}

#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn writable_roots(profile: SandboxProfile, root: &Path) -> Vec<PathBuf> {
    let mut roots = scratch_write_roots();
    if profile == SandboxProfile::WorkspaceWrite {
        roots.extend(toolchain_write_roots());
    }
    let mut roots = existing_safe_roots(roots);
    if profile == SandboxProfile::WorkspaceWrite {
        roots.extend(canonical_explicit_roots([root.to_path_buf()]));
    }
    roots.sort();
    roots.dedup();
    roots
}

/// Whether the profile requests any confinement at all.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn confines(profile: SandboxProfile) -> bool {
    !matches!(profile, SandboxProfile::DangerFullAccess)
}

/// Build a tokio Command for `program` with OS-level confinement applied for
/// the given profile. Subsequent `.arg()` calls on the returned command append
/// the program's own arguments (on macOS the real program is wrapped, but the
/// argument ordering is preserved so callers do not need to special-case it).
pub(crate) fn tokio_command(
    program: &str,
    profile: SandboxProfile,
    root: &Path,
    extra_read_roots: &[PathBuf],
) -> tokio::process::Command {
    #[cfg(target_os = "macos")]
    {
        if confines(profile) && macos::sandbox_exec_available() {
            let profile_text = macos::profile_text(profile, root, extra_read_roots)
                .unwrap_or_else(|| "(version 1)\n(deny default)\n".to_string());
            let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
            cmd.arg("-p").arg(profile_text).arg(program);
            return cmd;
        }
    }

    let mut cmd = tokio::process::Command::new(program);

    #[cfg(target_os = "linux")]
    if confines(profile) {
        linux::install_landlock_pre_exec(&mut cmd, profile, root, extra_read_roots);
    }

    let _ = (profile, root, extra_read_roots);
    cmd
}

/// Human-readable description of the active enforcement mechanism, for
/// `dext doctor` and startup diagnostics.
pub(crate) fn describe() -> String {
    #[cfg(target_os = "linux")]
    {
        match linux::landlock_abi() {
            Some(abi) if abi >= 1 => {
                format!("Linux Landlock (kernel ABI v{abi}, enforced)")
            }
            _ => "Linux: Landlock unavailable (kernel lacks the LSM); path-validation only"
                .to_string(),
        }
    }
    #[cfg(target_os = "macos")]
    {
        if macos::sandbox_exec_available() {
            "macOS Seatbelt (sandbox-exec, enforced)".to_string()
        } else {
            "macOS: sandbox-exec not found; path-validation only".to_string()
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        "this platform: no kernel sandbox; path-validation only (use WSL for enforced bash)"
            .to_string()
    }
}

/// Whether kernel-level enforcement is actually active right now.
pub(crate) fn is_enforced() -> bool {
    #[cfg(target_os = "linux")]
    {
        matches!(linux::landlock_abi(), Some(abi) if abi >= 1)
    }
    #[cfg(target_os = "macos")]
    {
        macos::sandbox_exec_available()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{linux_readable_roots, writable_roots};
    use crate::SandboxProfile;
    use landlock::{
        ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        path_beneath_rules,
    };
    use std::path::Path;

    /// Query the kernel's supported Landlock ABI without restricting anything.
    /// Returns None if Landlock is unavailable.
    pub(super) fn landlock_abi() -> Option<i64> {
        // landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)
        // returns the supported ABI version, or -1 with ENOSYS/EOPNOTSUPP.
        const LANDLOCK_CREATE_RULESET_VERSION: libc::c_ulong = 1 << 0;
        let ret = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        (ret >= 1).then_some(ret)
    }

    /// Build a fully-populated Landlock ruleset (all filesystem fds opened) so
    /// that the only work left for the forked child is the `restrict_self`
    /// syscall. Building before fork avoids allocating between fork and execve.
    fn build_ruleset(
        profile: SandboxProfile,
        root: &Path,
        extra_read_roots: &[std::path::PathBuf],
    ) -> Option<landlock::RulesetCreated> {
        let readable = linux_readable_roots(root, extra_read_roots);
        let writable = writable_roots(profile, root);

        let abi = ABI::from(landlock_abi()? as i32);
        let created = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .ok()?
            .create()
            .ok()?
            .add_rules(path_beneath_rules(&readable, AccessFs::from_read(abi)))
            .ok()?
            .add_rules(path_beneath_rules(&writable, AccessFs::from_all(abi)))
            .ok()?;

        Some(created)
    }

    pub(super) fn install_landlock_pre_exec(
        cmd: &mut tokio::process::Command,
        profile: SandboxProfile,
        root: &Path,
        extra_read_roots: &[std::path::PathBuf],
    ) {
        // Skip the work entirely if the kernel can't enforce Landlock.
        if landlock_abi().is_none() {
            return;
        }
        // Once a Landlock-capable kernel is detected, a requested confined
        // profile must not silently run unconfined because rule construction or
        // restriction failed.
        let Some(ruleset) = build_ruleset(profile, root, extra_read_roots) else {
            unsafe {
                cmd.pre_exec(|| Err(std::io::Error::from_raw_os_error(libc::EPERM)));
            }
            return;
        };

        let mut ruleset = Some(ruleset);
        unsafe {
            cmd.pre_exec(move || {
                let Some(ruleset) = ruleset.take() else {
                    return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                };
                match ruleset.restrict_self() {
                    Ok(status) if status.ruleset != RulesetStatus::NotEnforced => Ok(()),
                    Ok(_) | Err(_) => Err(std::io::Error::from_raw_os_error(libc::EPERM)),
                }
            });
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{home_safe_read_roots, sandbox_home_dir, sbpl_filter, sbpl_path, writable_roots};
    use crate::SandboxProfile;
    use std::path::Path;
    use std::sync::OnceLock;

    pub(super) fn sandbox_exec_available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| std::path::Path::new("/usr/bin/sandbox-exec").is_file())
    }

    /// Generate a Seatbelt (SBPL) profile that hides unrelated files below the
    /// user's home and limits writes to the selected profile's roots.
    pub(super) fn profile_text(
        profile: SandboxProfile,
        root: &Path,
        extra_read_roots: &[std::path::PathBuf],
    ) -> Option<String> {
        let home = std::fs::canonicalize(sandbox_home_dir()).ok()?;
        let escaped_home = sbpl_path(&home)?;
        let mut text = format!(
            "(version 1)\n(allow default)\n(deny file-read* (subpath \"{escaped_home}\"))\n(deny file-write*)\n"
        );
        let mut reads = String::new();
        for path in home_safe_read_roots(root, extra_read_roots) {
            let Some(filter) = sbpl_filter(&path) else {
                continue;
            };
            reads.push_str(&filter);
        }
        if !reads.is_empty() {
            text.push_str("(allow file-read*\n");
            text.push_str(&reads);
            text.push_str(")\n");
        }
        let mut allows = String::new();
        for path in writable_roots(profile, root) {
            let Some(filter) = sbpl_filter(&path) else {
                continue;
            };
            allows.push_str(&filter);
        }
        // /dev/null and friends are needed by virtually every command.
        allows.push_str("    (literal \"/dev/null\")\n    (literal \"/dev/tty\")\n");
        text.push_str("(allow file-write*\n");
        text.push_str(&allows);
        text.push_str(")\n");
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dext-sandbox-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).expect("create sandbox test directory");
        path
    }

    #[test]
    fn sbpl_paths_escape_syntax_and_reject_control_characters() {
        assert_eq!(
            sbpl_path(Path::new("/tmp/a\\b\"c")),
            Some("/tmp/a\\\\b\\\"c".to_string())
        );
        assert_eq!(sbpl_path(Path::new("/tmp/line\nbreak")), None);
    }

    #[test]
    fn sbpl_filters_distinguish_files_and_directories() {
        let root = temp_dir("sbpl-filter");
        let file = root.join("helper");
        std::fs::write(&file, "helper").expect("write helper");

        assert!(
            sbpl_filter(&root)
                .expect("directory filter")
                .contains("(subpath ")
        );
        assert!(
            sbpl_filter(&file)
                .expect("file filter")
                .contains("(literal ")
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn home_read_roots_exclude_unrelated_project_session_state() {
        let _guard = crate::test_env_lock().lock().expect("environment lock");
        let old_home = std::env::var_os("HOME");
        let old_userprofile = std::env::var_os("USERPROFILE");
        let old_dext_home = std::env::var_os("DEXT_HOME");
        let base = temp_dir("home-roots");
        let home = base.join("home");
        let state = home.join(".dext");
        let root = home.join("workspace");
        let extra = home.join("helpers/askpass");
        for directory in [
            state.join("packs"),
            state.join("shelves"),
            state.join("projects/other-session"),
            root.clone(),
            extra.parent().expect("extra parent").to_path_buf(),
        ] {
            std::fs::create_dir_all(directory).expect("create selected home root");
        }
        std::fs::write(&extra, "helper").expect("write helper");
        let conflicting_userprofile = base.join("not-the-unix-home");
        std::fs::create_dir_all(&conflicting_userprofile).expect("create conflicting userprofile");
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("USERPROFILE", &conflicting_userprofile);
            std::env::set_var("DEXT_HOME", &state);
        }

        assert_eq!(sandbox_home_dir(), home);
        let roots = home_safe_read_roots(&root, std::slice::from_ref(&extra));
        let writable = writable_roots(SandboxProfile::WorkspaceWrite, &home);
        let canonical = |path: &Path| std::fs::canonicalize(path).expect("canonical test path");
        assert!(roots.contains(&canonical(&root)));
        assert!(writable.contains(&canonical(&home)));
        assert!(roots.contains(&canonical(&state.join("packs"))));
        assert!(roots.contains(&canonical(&state.join("shelves"))));
        assert!(roots.contains(&canonical(&extra)));
        assert!(!roots.contains(&canonical(&state.join("projects/other-session"))));

        restore_env("HOME", old_home);
        restore_env("USERPROFILE", old_userprofile);
        restore_env("DEXT_HOME", old_dext_home);
        let _ = std::fs::remove_dir_all(base);
    }
}
