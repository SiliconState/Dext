//! Child process-tree lifecycle: detaching children into their own session so
//! they cannot reach the TUI's terminal, and tearing the whole tree down when
//! the root exits.

use std::io;
use std::process::Command;

// Children run in a new *session*, not merely a new process group: setsid()
// also detaches them from Dext's controlling terminal, so nothing they spawn
// can read from or paint over the TUI via /dev/tty (git credential prompts
// did exactly that — the prompt text garbled the input box while git hung on
// a terminal read that could never be answered). setsid() implies a fresh
// process group with pgid == pid, so the pgid-based cleanup in
// terminate_process_group_after_exit keeps working unchanged; setpgid is the
// fallback if setsid is ever refused.
#[cfg(unix)]
fn detach_session_pre_exec() -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    || {
        let setsid_result = unsafe { libc::setsid() };
        if setsid_result != -1 {
            return Ok(());
        }
        if unsafe { libc::setpgid(0, 0) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(unix)]
pub(crate) fn configure_std_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    unsafe {
        cmd.pre_exec(detach_session_pre_exec());
    }
}

#[cfg(windows)]
pub(crate) fn configure_std_process_group(cmd: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn configure_std_process_group(_cmd: &mut Command) {}

#[cfg(unix)]
pub(crate) fn configure_tokio_process_group(cmd: &mut tokio::process::Command) {
    unsafe {
        cmd.pre_exec(detach_session_pre_exec());
    }
}

#[cfg(windows)]
pub(crate) fn configure_tokio_process_group(cmd: &mut tokio::process::Command) {
    cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn configure_tokio_process_group(_cmd: &mut tokio::process::Command) {}

#[cfg(windows)]
fn resume_windows_process(pid: u32) -> io::Result<()> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
        },
    };

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut present = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while present {
        if entry.th32OwnerProcessID == pid {
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if thread.is_null() {
                let error = io::Error::last_os_error();
                unsafe {
                    CloseHandle(snapshot);
                }
                return Err(error);
            }
            let resumed = unsafe { ResumeThread(thread) };
            let error = (resumed == u32::MAX).then(io::Error::last_os_error);
            unsafe {
                CloseHandle(thread);
                CloseHandle(snapshot);
            }
            return error.map_or(Ok(()), Err);
        }
        present = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    unsafe {
        CloseHandle(snapshot);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "suspended child thread was not found",
    ))
}

pub(crate) struct ChildProcessTree {
    #[cfg(unix)]
    pid: u32,
    #[cfg(unix)]
    armed: std::sync::atomic::AtomicBool,
    #[cfg(windows)]
    job: usize,
}

impl ChildProcessTree {
    pub(crate) fn for_std(child: &std::process::Child) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                pid: child.id(),
                armed: std::sync::atomic::AtomicBool::new(true),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            Self::for_windows_process(child.as_raw_handle() as _, child.id())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Ok(Self {})
        }
    }

    pub(crate) fn for_tokio(child: &tokio::process::Child) -> io::Result<Self> {
        #[cfg(unix)]
        {
            child
                .id()
                .map(|pid| Self {
                    pid,
                    armed: std::sync::atomic::AtomicBool::new(true),
                })
                .ok_or_else(|| io::Error::other("child exited before process-tree setup"))
        }
        #[cfg(windows)]
        {
            let process = child
                .raw_handle()
                .ok_or_else(|| io::Error::other("child exited before process-tree setup"))?;
            let pid = child
                .id()
                .ok_or_else(|| io::Error::other("child exited before process-tree setup"))?;
            Self::for_windows_process(process as _, pid)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child;
            Ok(Self {})
        }
    }

    #[cfg(windows)]
    fn for_windows_process(
        process: windows_sys::Win32::Foundation::HANDLE,
        pid: u32,
    ) -> io::Result<Self> {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
                SetInformationJobObject,
            },
        };

        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if configured == 0 {
            let error = io::Error::last_os_error();
            unsafe {
                CloseHandle(job);
            }
            return Err(error);
        }
        if unsafe { AssignProcessToJobObject(job, process) } == 0 {
            let error = io::Error::last_os_error();
            unsafe {
                CloseHandle(job);
            }
            return Err(error);
        }
        if let Err(error) = resume_windows_process(pid) {
            unsafe {
                CloseHandle(job);
            }
            return Err(error);
        }
        Ok(Self { job: job as usize })
    }

    pub(crate) fn terminate_after_root_exit(&self) {
        #[cfg(unix)]
        {
            signal_process_group(self.pid, libc::SIGTERM);
            signal_process_group(self.pid, libc::SIGKILL);
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job as _, 1);
        }
        #[cfg(unix)]
        self.armed
            .store(false, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn terminate_std_child(&self, child: &mut std::process::Child) {
        #[cfg(unix)]
        {
            signal_process_group(self.pid, libc::SIGTERM);
            std::thread::sleep(std::time::Duration::from_millis(50));
            signal_process_group(self.pid, libc::SIGKILL);
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job as _, 1);
        }
        let _ = child.kill();
        let _ = child.wait();
        #[cfg(unix)]
        self.armed
            .store(false, std::sync::atomic::Ordering::Release);
    }

    pub(crate) async fn terminate_tokio_child(&self, child: &mut tokio::process::Child) {
        #[cfg(unix)]
        {
            signal_process_group(self.pid, libc::SIGTERM);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            signal_process_group(self.pid, libc::SIGKILL);
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job as _, 1);
        }
        let _ = child.kill().await;
        let _ = child.wait().await;
        #[cfg(unix)]
        self.armed
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

impl Drop for ChildProcessTree {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.armed.load(std::sync::atomic::Ordering::Acquire) {
            signal_process_group(self.pid, libc::SIGTERM);
            signal_process_group(self.pid, libc::SIGKILL);
        }
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job as _);
        }
    }
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: libc::c_int) {
    let pgid = -(pid as libc::pid_t);
    unsafe {
        let _ = libc::kill(pgid, signal);
    }
}
