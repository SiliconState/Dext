// OS-level filesystem sandboxing for tool subprocesses.
//
// This is defense-in-depth that backstops the heuristic command-risk
// classifier and the path-validation layer: even if a command is
// misclassified as read-only, the kernel still blocks writes outside the
// permitted roots. It is best-effort by design — if the platform cannot
// enforce confinement, command execution proceeds unsandboxed rather than
// failing, and `describe()` reports the true status so it is never silently
// assumed.
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
//                      writable so heredocs and well-behaved filters work.
//   - WorkspaceWrite:  additionally permit writes under the sandbox root and
//                      the user's home directory. Home is allowed because
//                      toolchains (cargo, npm, pip, go, ...) write per-user
//                      caches outside the workspace; the boundary that matters
//                      in write-mode is the system directories (/etc, /usr,
//                      /bin, ...), which stay denied.
//   - DangerFullAccess: no confinement.

use std::path::{Path, PathBuf};

use crate::SandboxProfile;

/// Paths that must stay writable under every profile: scratch space (heredocs,
/// lock files) and the device nodes virtually all commands need — `/dev/null`,
/// `/dev/tty`, the controlling terminal, etc. Denying `/dev/null` writes alone
/// breaks `2>/dev/null`, git, and most build tools.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn scratch_write_roots() -> Vec<PathBuf> {
    let mut roots = vec![std::env::temp_dir()];
    for p in ["/tmp", "/var/tmp", "/dev/shm", "/dev"] {
        let path = PathBuf::from(p);
        if path.is_dir() {
            roots.push(path);
        }
    }
    roots
}

#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn writable_roots(profile: SandboxProfile, root: &Path) -> Vec<PathBuf> {
    let mut roots = scratch_write_roots();
    if profile == SandboxProfile::WorkspaceWrite {
        roots.push(root.to_path_buf());
        // Toolchains (cargo, npm, pip, go, ...) write to per-user caches and
        // config outside the workspace; without the home directory, common
        // verification commands like `cargo test` fail to take their package
        // cache lock. System directories (/etc, /usr, /bin, ...) stay denied —
        // that is the boundary that matters for write-mode. The strict
        // read-only profile excludes the home directory by design.
        roots.push(crate::session::user_home_dir());
    }
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
) -> tokio::process::Command {
    #[cfg(target_os = "macos")]
    {
        if confines(profile)
            && let Some(profile_text) = macos::profile_text(profile, root)
            && macos::sandbox_exec_available()
        {
            let mut cmd = tokio::process::Command::new("sandbox-exec");
            cmd.arg("-p").arg(profile_text).arg(program);
            return cmd;
        }
    }

    let mut cmd = tokio::process::Command::new(program);

    #[cfg(target_os = "linux")]
    if confines(profile) {
        linux::install_landlock_pre_exec(&mut cmd, profile, root);
    }

    let _ = (profile, root);
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
    use super::writable_roots;
    use crate::SandboxProfile;
    use landlock::{
        ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, path_beneath_rules,
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
    fn build_ruleset(profile: SandboxProfile, root: &Path) -> Option<landlock::RulesetCreated> {
        let writable: Vec<_> = writable_roots(profile, root)
            .into_iter()
            .filter(|p| p.exists())
            .collect();

        let abi = ABI::from(landlock_abi()? as i32);
        let created = Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .ok()?
            .create()
            .ok()?
            // Reads are permitted everywhere; only writes are confined.
            .add_rules(path_beneath_rules(["/"], AccessFs::from_read(abi)))
            .ok()?
            .add_rules(path_beneath_rules(&writable, AccessFs::from_all(abi)))
            .ok()?;

        Some(created)
    }

    pub(super) fn install_landlock_pre_exec(
        cmd: &mut tokio::process::Command,
        profile: SandboxProfile,
        root: &Path,
    ) {
        // Skip the work entirely if the kernel can't enforce Landlock.
        if landlock_abi().is_none() {
            return;
        }
        let Some(ruleset) = build_ruleset(profile, root) else {
            return;
        };

        // restrict_self consumes the ruleset and applies to the calling thread;
        // after fork the child is single-threaded, and the restriction is
        // preserved across execve. All errors are swallowed so a sandbox
        // failure can never block command execution.
        let mut ruleset = Some(ruleset);
        unsafe {
            cmd.pre_exec(move || {
                if let Some(rs) = ruleset.take() {
                    let _ = rs.restrict_self();
                }
                Ok(())
            });
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::writable_roots;
    use crate::SandboxProfile;
    use std::path::Path;
    use std::sync::OnceLock;

    pub(super) fn sandbox_exec_available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| std::path::Path::new("/usr/bin/sandbox-exec").exists())
    }

    fn escape_sbpl(path: &Path) -> String {
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }

    /// Generate a Seatbelt (SBPL) profile: allow everything, deny all writes,
    /// then re-allow writes under the permitted roots. SBPL is last-match-wins.
    pub(super) fn profile_text(profile: SandboxProfile, root: &Path) -> Option<String> {
        let mut text = String::from("(version 1)\n(allow default)\n(deny file-write*)\n");
        let mut allows = String::new();
        for path in writable_roots(profile, root) {
            let Ok(canonical) = std::fs::canonicalize(&path) else {
                continue;
            };
            allows.push_str(&format!("    (subpath \"{}\")\n", escape_sbpl(&canonical)));
        }
        // /dev/null and friends are needed by virtually every command.
        allows.push_str("    (literal \"/dev/null\")\n    (literal \"/dev/tty\")\n");
        text.push_str("(allow file-write*\n");
        text.push_str(&allows);
        text.push_str(")\n");
        Some(text)
    }
}
