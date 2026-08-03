// OS-level filesystem sandboxing for tool subprocesses.
//
// This is defense-in-depth that backstops the heuristic command-risk
// classifier and native-tool path validation. Unsupported platforms keep native
// path guards, but tool subprocesses run unconfined; once a kernel mechanism is
// selected, failures stop the child instead of silently running it unconfined.
//
// Enforcement by platform:
//   - Linux:   Landlock LSM (kernel >= 5.13 with the LSM enabled).
//   - macOS:   Seatbelt via `sandbox-exec` wrapping.
//   - Windows: not enforced at the kernel level here (AppContainer would
//              break common Git-Bash installs); native tool path guards remain,
//              but shell/external subprocesses are unconfined. Use WSL for
//              kernel-enforced bash.
//
// Profiles:
//   - ReadOnly:        allow every read the Dext process user can perform, but
//                      deny writes except to scratch roots and required device
//                      nodes.
//   - WorkspaceWrite:  additionally permit writes under the sandbox root and
//                      common per-user toolchain cache roots.
//   - DangerFullAccess: no confinement.

use std::ffi::OsStr;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

use crate::SandboxProfile;

pub(crate) struct PrivateScratch {
    pub(crate) path: PathBuf,
}

impl PrivateScratch {
    pub(crate) fn create() -> std::io::Result<Self> {
        #[cfg(target_os = "linux")]
        let bases = [PathBuf::from("/tmp"), PathBuf::from("/var/tmp")];
        #[cfg(target_os = "macos")]
        let bases = [std::env::temp_dir(), PathBuf::from("/private/tmp")];
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let bases = [std::env::temp_dir()];

        for base in bases {
            let Ok(base) = std::fs::canonicalize(base) else {
                continue;
            };
            for _ in 0..16 {
                let mut nonce = [0u8; 16];
                if getrandom::fill(&mut nonce).is_err() {
                    return Err(std::io::Error::other(
                        "could not generate sandbox scratch nonce",
                    ));
                }
                let nonce = nonce
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                let path = base.join(format!("dext-sandbox-{}-{nonce}", std::process::id()));
                let mut builder = std::fs::DirBuilder::new();
                builder.recursive(false);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt as _;
                    builder.mode(0o700);
                }
                match builder.create(&path) {
                    Ok(()) => {
                        let validation = (|| -> std::io::Result<PathBuf> {
                            #[cfg(unix)]
                            {
                                use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
                                let metadata = std::fs::symlink_metadata(&path)?;
                                if metadata.file_type().is_symlink()
                                    || !metadata.is_dir()
                                    || metadata.uid() != unsafe { libc::geteuid() }
                                {
                                    return Err(std::io::Error::other(
                                        "sandbox scratch directory failed ownership validation",
                                    ));
                                }
                                std::fs::set_permissions(
                                    &path,
                                    std::fs::Permissions::from_mode(0o700),
                                )?;
                            }
                            let canonical = std::fs::canonicalize(&path)?;
                            if canonical.parent() != Some(base.as_path()) {
                                return Err(std::io::Error::other(
                                    "sandbox scratch directory escaped its base",
                                ));
                            }
                            Ok(canonical)
                        })();
                        match validation {
                            Ok(path) => return Ok(Self { path }),
                            Err(error) => {
                                let _ = std::fs::remove_dir_all(&path);
                                return Err(error);
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(_) => break,
                }
            }
        }
        Err(std::io::Error::other(
            "could not create private sandbox scratch directory",
        ))
    }
}

impl Drop for PrivateScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

pub(crate) struct SandboxedCommand {
    command: tokio::process::Command,
    _scratch: Option<PrivateScratch>,
}

pub(crate) struct SandboxedStdCommand {
    command: std::process::Command,
    scratch: Option<PrivateScratch>,
}

impl SandboxedStdCommand {
    pub(crate) fn scratch_path(&self) -> Option<&Path> {
        self.scratch.as_ref().map(|scratch| scratch.path.as_path())
    }

    pub(crate) fn into_parts(self) -> (std::process::Command, Option<PrivateScratch>) {
        (self.command, self.scratch)
    }
}

impl Deref for SandboxedStdCommand {
    type Target = std::process::Command;

    fn deref(&self) -> &Self::Target {
        &self.command
    }
}

impl DerefMut for SandboxedStdCommand {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.command
    }
}

impl Deref for SandboxedCommand {
    type Target = tokio::process::Command;

    fn deref(&self) -> &Self::Target {
        &self.command
    }
}

impl DerefMut for SandboxedCommand {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.command
    }
}

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

/// Paths that must stay writable under every profile: the per-command private
/// scratch directory and the small set of device nodes ordinary commands need.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn scratch_write_roots(scratch: &Path) -> Vec<PathBuf> {
    let mut roots = vec![scratch.to_path_buf()];
    let null = PathBuf::from("/dev/null");
    if null.exists() {
        roots.push(null);
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
fn writable_roots(profile: SandboxProfile, root: &Path, scratch: &Path) -> Vec<PathBuf> {
    let mut roots = scratch_write_roots(scratch);
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

pub(crate) fn std_command(
    program: impl AsRef<OsStr>,
    profile: SandboxProfile,
    root: &Path,
) -> std::io::Result<SandboxedStdCommand> {
    std_command_inner(program.as_ref(), profile, root, false)
}

pub(crate) fn std_command_offline(
    program: impl AsRef<OsStr>,
    profile: SandboxProfile,
    root: &Path,
) -> std::io::Result<SandboxedStdCommand> {
    std_command_inner(program.as_ref(), profile, root, true)
}

fn std_command_inner(
    program: &OsStr,
    profile: SandboxProfile,
    root: &Path,
    offline: bool,
) -> std::io::Result<SandboxedStdCommand> {
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    if offline {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "offline subprocess confinement is unavailable on this platform",
        ));
    }
    #[cfg(target_os = "linux")]
    if offline && confines(profile) && linux::landlock_abi().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "offline read-only subprocess confinement requires Landlock",
        ));
    }
    #[cfg(all(
        target_os = "linux",
        not(any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "riscv64"
        ))
    ))]
    if offline {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "offline subprocess confinement is unavailable on this Linux architecture",
        ));
    }

    let scratch = Some(PrivateScratch::create()?);

    #[cfg(target_os = "macos")]
    let mut command = if confines(profile) && macos::sandbox_exec_available() {
        let scratch_path = scratch
            .as_ref()
            .map(|scratch| scratch.path.as_path())
            .ok_or_else(|| std::io::Error::other("confined command has no private scratch"))?;
        let profile_text = macos::profile_text(profile, root, scratch_path, offline)
            .ok_or_else(|| std::io::Error::other("could not build macOS sandbox profile"))?;
        let mut command = std::process::Command::new("/usr/bin/sandbox-exec");
        command.arg("-p").arg(profile_text).arg(program);
        command
    } else if offline {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "offline subprocess confinement requires sandbox-exec",
        ));
    } else {
        std::process::Command::new(program)
    };

    #[cfg(not(target_os = "macos"))]
    let mut command = std::process::Command::new(program);

    #[cfg(target_os = "linux")]
    {
        if confines(profile) {
            let scratch_path = scratch
                .as_ref()
                .map(|scratch| scratch.path.as_path())
                .ok_or_else(|| std::io::Error::other("confined command has no private scratch"))?;
            linux::install_landlock_pre_exec_std(&mut command, profile, root, scratch_path);
        }
        if offline {
            linux::install_offline_pre_exec_std(&mut command);
        }
    }

    if let Some(scratch) = scratch.as_ref() {
        command
            .env("TMPDIR", &scratch.path)
            .env("TMP", &scratch.path)
            .env("TEMP", &scratch.path);
    }
    let _ = (profile, root, offline);
    Ok(SandboxedStdCommand { command, scratch })
}

/// Build a tokio Command for `program` with OS-level confinement applied for
/// the given profile. Subsequent `.arg()` calls on the returned command append
/// the program's own arguments (on macOS the real program is wrapped, but the
/// argument ordering is preserved so callers do not need to special-case it).
pub(crate) fn tokio_command(
    program: impl AsRef<OsStr>,
    profile: SandboxProfile,
    root: &Path,
) -> std::io::Result<SandboxedCommand> {
    let program = program.as_ref();
    let scratch = Some(PrivateScratch::create()?);

    #[cfg(target_os = "macos")]
    let mut command = if confines(profile) && macos::sandbox_exec_available() {
        let scratch_path = scratch
            .as_ref()
            .map(|scratch| scratch.path.as_path())
            .ok_or_else(|| std::io::Error::other("confined command has no private scratch"))?;
        let profile_text = macos::profile_text(profile, root, scratch_path, false)
            .ok_or_else(|| std::io::Error::other("could not build macOS sandbox profile"))?;
        let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
        command.arg("-p").arg(profile_text).arg(program);
        command
    } else {
        tokio::process::Command::new(program)
    };

    #[cfg(not(target_os = "macos"))]
    let mut command = tokio::process::Command::new(program);

    #[cfg(target_os = "linux")]
    if confines(profile) {
        let scratch_path = scratch
            .as_ref()
            .map(|scratch| scratch.path.as_path())
            .ok_or_else(|| std::io::Error::other("confined command has no private scratch"))?;
        linux::install_landlock_pre_exec(&mut command, profile, root, scratch_path);
    }

    if let Some(scratch) = scratch.as_ref() {
        command
            .env("TMPDIR", &scratch.path)
            .env("TMP", &scratch.path)
            .env("TEMP", &scratch.path);
    }
    let _ = (profile, root);
    Ok(SandboxedCommand {
        command,
        _scratch: scratch,
    })
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
            _ => "Linux: Landlock unavailable (kernel lacks the LSM); tool subprocesses are unconfined, native path guards only"
                .to_string(),
        }
    }
    #[cfg(target_os = "macos")]
    {
        if macos::sandbox_exec_available() {
            "macOS Seatbelt (sandbox-exec, enforced)".to_string()
        } else {
            "macOS: sandbox-exec not found; tool subprocesses are unconfined, native path guards only".to_string()
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        "this platform: no kernel sandbox; tool subprocesses are unconfined, native path guards only (use WSL for enforced bash)"
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
        ABI, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, path_beneath_rules,
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
        scratch: &Path,
    ) -> Option<landlock::RulesetCreated> {
        let writable = writable_roots(profile, root, scratch);
        let abi = ABI::from(landlock_abi()? as i32);
        let write_access = AccessFs::from_write(abi);
        let created = Ruleset::default()
            .handle_access(write_access)
            .ok()?
            .create()
            .ok()?
            .add_rules(path_beneath_rules(&writable, write_access))
            .ok()?;

        Some(created)
    }

    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    ))]
    fn install_offline_filter() -> std::io::Result<()> {
        const SECCOMP_DATA_NR_OFFSET: u32 = 0;
        const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;
        #[cfg(target_arch = "x86_64")]
        const AUDIT_ARCH: u32 = 0xc000_003e;
        #[cfg(target_arch = "aarch64")]
        const AUDIT_ARCH: u32 = 0xc000_00b7;
        #[cfg(target_arch = "riscv64")]
        const AUDIT_ARCH: u32 = 0xc000_00f3;
        #[cfg(target_arch = "x86_64")]
        const FIRST_FORBIDDEN_ABI_NR: u32 = 0x4000_0000;
        #[cfg(not(target_arch = "x86_64"))]
        const FIRST_FORBIDDEN_ABI_NR: u32 = u32::MAX;

        let statement = |code: u16, k: u32| libc::sock_filter {
            code,
            jt: 0,
            jf: 0,
            k,
        };
        let jump = |code: u16, k: u32, jt: u8, jf: u8| libc::sock_filter { code, jt, jf, k };
        let load = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
        let equal = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
        let at_least = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
        let ret = (libc::BPF_RET | libc::BPF_K) as u16;
        let deny = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
        let mut filter = [
            statement(load, SECCOMP_DATA_ARCH_OFFSET),
            jump(equal, AUDIT_ARCH, 1, 0),
            statement(ret, libc::SECCOMP_RET_KILL_PROCESS),
            statement(load, SECCOMP_DATA_NR_OFFSET),
            jump(at_least, FIRST_FORBIDDEN_ABI_NR, 0, 1),
            statement(ret, libc::SECCOMP_RET_KILL_PROCESS),
            jump(equal, libc::SYS_socket as u32, 0, 1),
            statement(ret, deny),
            jump(equal, libc::SYS_io_uring_setup as u32, 0, 1),
            statement(ret, deny),
            jump(equal, libc::SYS_pidfd_getfd as u32, 0, 1),
            statement(ret, deny),
            jump(equal, libc::SYS_ptrace as u32, 0, 1),
            statement(ret, deny),
            statement(ret, libc::SECCOMP_RET_ALLOW),
        ];
        let program = libc::sock_fprog {
            len: filter.len() as libc::c_ushort,
            filter: filter.as_mut_ptr(),
        };

        let no_new_privileges = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if no_new_privileges != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let restricted = unsafe {
            libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &program as *const libc::sock_fprog,
            )
        };
        if restricted != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    #[cfg(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    ))]
    pub(super) fn install_offline_pre_exec_std(cmd: &mut std::process::Command) {
        use std::os::unix::process::CommandExt as _;

        unsafe {
            cmd.pre_exec(install_offline_filter);
        }
    }

    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )))]
    pub(super) fn install_offline_pre_exec_std(cmd: &mut std::process::Command) {
        use std::os::unix::process::CommandExt as _;

        unsafe {
            cmd.pre_exec(|| Err(std::io::Error::from_raw_os_error(libc::EPERM)));
        }
    }

    pub(super) fn install_landlock_pre_exec_std(
        cmd: &mut std::process::Command,
        profile: SandboxProfile,
        root: &Path,
        scratch: &Path,
    ) {
        use std::os::unix::process::CommandExt as _;

        if landlock_abi().is_none() {
            return;
        }
        let Some(ruleset) = build_ruleset(profile, root, scratch) else {
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

    pub(super) fn install_landlock_pre_exec(
        cmd: &mut tokio::process::Command,
        profile: SandboxProfile,
        root: &Path,
        scratch: &Path,
    ) {
        // Skip the work entirely if the kernel can't enforce Landlock.
        if landlock_abi().is_none() {
            return;
        }
        // Once a Landlock-capable kernel is detected, a requested confined
        // profile must not silently run unconfined because rule construction or
        // restriction failed.
        let Some(ruleset) = build_ruleset(profile, root, scratch) else {
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
    use super::{sbpl_filter, writable_roots};
    use crate::SandboxProfile;
    use std::path::Path;
    use std::sync::OnceLock;

    pub(super) fn sandbox_exec_available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| std::path::Path::new("/usr/bin/sandbox-exec").is_file())
    }

    /// Generate a Seatbelt (SBPL) profile that preserves reads and limits
    /// writes to the selected profile's roots.
    pub(super) fn profile_text(
        profile: SandboxProfile,
        root: &Path,
        scratch: &Path,
        offline: bool,
    ) -> Option<String> {
        let mut text = "(version 1)\n(allow default)\n(deny file-write*)\n".to_string();
        if offline {
            text.push_str("(deny network*)\n");
        }
        let mut allows = String::new();
        for path in writable_roots(profile, root, scratch) {
            let mut aliases = vec![path.clone()];
            if let Ok(relative) = path.strip_prefix("/private") {
                let alias = Path::new("/").join(relative);
                if alias.starts_with("/var") || alias.starts_with("/tmp") {
                    aliases.push(alias);
                }
            }
            for alias in aliases {
                let Some(filter) = sbpl_filter(&alias) else {
                    continue;
                };
                allows.push_str(&filter);
            }
        }
        text.push_str("(allow file-write*\n");
        text.push_str(&allows);
        text.push_str(")\n");
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn private_scratch_is_owned_private_and_removed_on_drop() {
        let path = {
            let scratch = PrivateScratch::create().expect("create private scratch");
            let path = scratch.path.clone();
            let metadata = std::fs::symlink_metadata(&path).expect("scratch metadata");
            assert!(metadata.is_dir());
            assert!(!metadata.file_type().is_symlink());
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
                assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
                assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
            }
            path
        };
        assert!(
            !path.exists(),
            "dropping the scratch guard must remove {}",
            path.display()
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn offline_socket_probe_child() {
        if std::env::var_os("DEXT_OFFLINE_SOCKET_PROBE").is_none() {
            return;
        }

        #[cfg(target_os = "linux")]
        for (domain, socket_type) in [
            (libc::AF_INET, libc::SOCK_STREAM),
            (libc::AF_INET, libc::SOCK_DGRAM),
            (libc::AF_INET6, libc::SOCK_STREAM),
            (libc::AF_INET6, libc::SOCK_DGRAM),
        ] {
            let fd = unsafe { libc::socket(domain, socket_type, 0) };
            if fd >= 0 {
                unsafe {
                    libc::close(fd);
                }
                panic!("offline child created network socket domain={domain} type={socket_type}");
            }
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::PermissionDenied,
                "offline socket failure must be an explicit sandbox denial"
            );
        }

        #[cfg(target_os = "macos")]
        {
            for address in ["127.0.0.1:9", "[::1]:9"] {
                let error = std::net::TcpStream::connect(address)
                    .expect_err("offline child opened a TCP connection");
                assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied,
                    "offline TCP failure must be an explicit sandbox denial for {address}: {error}"
                );
            }
            for address in ["127.0.0.1:0", "[::1]:0"] {
                let error = std::net::UdpSocket::bind(address)
                    .expect_err("offline child bound a UDP socket");
                assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied,
                    "offline UDP failure must be an explicit sandbox denial for {address}: {error}"
                );
            }
        }
        println!("offline socket probe passed");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn offline_command_denies_tcp_and_udp_after_exec() {
        if !is_enforced() {
            eprintln!("skipping offline socket enforcement matrix: {}", describe());
            return;
        }
        let root = temp_dir("offline-network");
        let current_exe = std::env::current_exe().expect("locate test executable");
        let mut command = std_command_offline(&current_exe, SandboxProfile::ReadOnly, &root)
            .expect("prepare offline child");
        let output = command
            .arg("--exact")
            .arg("sandbox::tests::offline_socket_probe_child")
            .arg("--nocapture")
            .env("DEXT_OFFLINE_SOCKET_PROBE", "1")
            .output()
            .expect("run offline child");
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("offline socket probe passed"),
            "captured pipe output must remain available"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn sandbox_adversarial_write_matrix_enforces_profile_boundaries() {
        if !is_enforced() {
            eprintln!("skipping sandbox adversarial write matrix: {}", describe());
            return;
        }

        let _guard = crate::test_env_lock();
        let old_home = std::env::var_os("HOME");
        let base = temp_dir("adversarial-write-matrix");
        let workspace = base.join("workspace");
        let home = base.join("home");
        let cache = home.join(".cache/pip");
        for directory in [&workspace, &home, &cache] {
            std::fs::create_dir_all(directory).expect("create matrix directory");
        }
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let workspace_target = workspace.join("workspace.txt");
        let cache_target = cache.join("cache.txt");
        let parent_target = base.join("parent.txt");
        let home_target = home.join("home.txt");
        let shared_temp_target = PathBuf::from("/tmp").join(format!(
            "dext-sandbox-shared-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let symlink_target = base.join("symlink-target.txt");
        let symlink_path = workspace.join("escape-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&symlink_target, &symlink_path).expect("create matrix symlink");

        struct WriteCase<'a> {
            label: &'a str,
            requested: &'a Path,
            observed: &'a Path,
            read_only: bool,
            workspace_write: bool,
        }

        let cases = [
            WriteCase {
                label: "workspace",
                requested: &workspace_target,
                observed: &workspace_target,
                read_only: false,
                workspace_write: true,
            },
            WriteCase {
                label: "toolchain-cache",
                requested: &cache_target,
                observed: &cache_target,
                read_only: false,
                workspace_write: true,
            },
            WriteCase {
                label: "workspace-parent",
                requested: &parent_target,
                observed: &parent_target,
                read_only: false,
                workspace_write: false,
            },
            WriteCase {
                label: "home",
                requested: &home_target,
                observed: &home_target,
                read_only: false,
                workspace_write: false,
            },
            WriteCase {
                label: "shared-temp",
                requested: &shared_temp_target,
                observed: &shared_temp_target,
                read_only: false,
                workspace_write: false,
            },
            WriteCase {
                label: "symlink-escape",
                requested: &symlink_path,
                observed: &symlink_target,
                read_only: false,
                workspace_write: false,
            },
        ];

        for case in &cases {
            std::fs::write(case.requested, b"control")
                .unwrap_or_else(|error| panic!("control write failed for {}: {error}", case.label));
            assert!(
                case.observed.exists(),
                "control did not create {}",
                case.label
            );
            std::fs::remove_file(case.observed).expect("remove matrix control target");
        }

        for profile in [SandboxProfile::ReadOnly, SandboxProfile::WorkspaceWrite] {
            for case in &cases {
                let _ = std::fs::remove_file(case.observed);
                let mut command =
                    std_command("bash", profile, &workspace).expect("prepare matrix command");
                let output = command
                    .current_dir(&workspace)
                    .arg("-c")
                    .arg("printf matrix > \"$1\" 2>/dev/null")
                    .arg("--")
                    .arg(case.requested)
                    .output()
                    .expect("run matrix command");
                let should_write = match profile {
                    SandboxProfile::ReadOnly => case.read_only,
                    SandboxProfile::WorkspaceWrite => case.workspace_write,
                    SandboxProfile::DangerFullAccess => unreachable!(),
                };
                assert_eq!(
                    output.status.success(),
                    should_write,
                    "{} under {profile:?}: status={} stderr={} ({})",
                    case.label,
                    output.status,
                    String::from_utf8_lossy(&output.stderr),
                    describe()
                );
                assert_eq!(
                    case.observed.exists(),
                    should_write,
                    "{} under {profile:?} had unexpected filesystem result ({})",
                    case.label,
                    describe()
                );
            }
        }

        unsafe {
            match old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = std::fs::remove_file(shared_temp_target);
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn confined_command_uses_one_private_temp_directory_and_cleans_it_up() {
        let root = temp_dir("private-temp-command");
        let mut command = tokio_command("bash", SandboxProfile::ReadOnly, &root)
            .expect("prepare confined command");
        let output = command
            .arg("-c")
            .arg(
                "test \"$TMPDIR\" = \"$TMP\" && test \"$TMPDIR\" = \"$TEMP\" && \
                 file=$(mktemp \"$TMPDIR/dext.XXXXXX\") && test -f \"$file\" && printf '%s\\n' \"$TMPDIR\"",
            )
            .output()
            .await
            .expect("run confined command");
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let scratch = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        assert!(scratch.is_dir(), "scratch must live through command output");
        assert!(
            scratch
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("dext-sandbox-")),
            "unexpected scratch path: {}",
            scratch.display()
        );
        drop(command);
        assert!(
            !scratch.exists(),
            "scratch must be removed after the command wrapper is dropped"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn writable_roots_exclude_unrelated_home_content() {
        let _guard = crate::test_env_lock();
        let old_home = std::env::var_os("HOME");
        let base = temp_dir("writable-roots");
        let home = base.join("home");
        let root = home.join("workspace");
        let cache = home.join(".cache/pip");
        let unrelated = home.join(".dext/projects/other-session");
        for directory in [&root, &cache, &unrelated] {
            std::fs::create_dir_all(directory).expect("create test directory");
        }
        unsafe {
            std::env::set_var("HOME", &home);
        }

        let scratch = base.join("scratch");
        let inherited_temp = base.join("inherited-temp");
        std::fs::create_dir_all(&scratch).expect("create scratch directory");
        std::fs::create_dir_all(&inherited_temp).expect("create inherited temp directory");
        let old_tmpdir = std::env::var_os("TMPDIR");
        unsafe {
            std::env::set_var("TMPDIR", &inherited_temp);
        }

        let read_only = writable_roots(SandboxProfile::ReadOnly, &root, &scratch);
        let workspace_write = writable_roots(SandboxProfile::WorkspaceWrite, &root, &scratch);
        let canonical = |path: &Path| std::fs::canonicalize(path).expect("canonical test path");
        assert!(read_only.contains(&canonical(&scratch)));
        assert!(workspace_write.contains(&canonical(&scratch)));
        assert!(!read_only.contains(&canonical(&root)));
        assert!(workspace_write.contains(&canonical(&root)));
        assert!(workspace_write.contains(&canonical(&cache)));
        assert!(!workspace_write.contains(&canonical(&unrelated)));
        assert!(!workspace_write.contains(&canonical(&inherited_temp)));
        for shared_temp in ["/tmp", "/var/tmp", "/dev/shm"] {
            if let Ok(shared_temp) = std::fs::canonicalize(shared_temp) {
                assert!(
                    !workspace_write.contains(&shared_temp),
                    "confinement must not permit shared temp root {}",
                    shared_temp.display()
                );
            }
        }
        for device in ["/dev/tty", "/dev/ptmx"] {
            if let Ok(device) = std::fs::canonicalize(device) {
                assert!(
                    !workspace_write.contains(&device),
                    "confinement must not permit output that bypasses capture: {}",
                    device.display()
                );
            }
        }
        if let Ok(dev_pts) = std::fs::canonicalize("/dev/pts") {
            assert!(
                !workspace_write.contains(&dev_pts),
                "confinement must not permit writing arbitrary pseudo-terminals"
            );
        }

        unsafe {
            match old_tmpdir {
                Some(value) => std::env::set_var("TMPDIR", value),
                None => std::env::remove_var("TMPDIR"),
            }
            match old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(base);
    }
}
