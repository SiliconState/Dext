#![cfg(windows)]

use std::ffi::{OsStr, OsString, c_void};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_BROKEN_PIPE, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

const COLS: i16 = 120;
const ROWS: i16 = 40;
const TIMEOUT: Duration = Duration::from_secs(15);
const OUTPUT_CAP: usize = 4 * 1024 * 1024;

#[test]
fn tui_smoke_launches_real_binary_in_conpty_and_restores_terminal() {
    let temp = TempDir::new("dext-conpty").expect("temp directory");
    let sandbox = temp.path.join("sandbox");
    let dext_home = temp.path.join("dext-home");
    let home = temp.path.join("home");
    std::fs::create_dir_all(&sandbox).expect("sandbox");
    std::fs::create_dir_all(&dext_home).expect("dext home");
    std::fs::create_dir_all(&home).expect("home");

    let mut conpty = ConPty::spawn(&sandbox, &dext_home, &home).expect("spawn Dext in ConPTY");
    conpty.wait_for("Dext", TIMEOUT).expect("read Dext banner");
    conpty
        .write_all(b"/status\r")
        .expect("write status command");
    conpty
        .wait_for("approval profile: always", TIMEOUT)
        .expect("read default approval status");
    conpty
        .wait_for("sandbox profile: danger-full-access", TIMEOUT)
        .expect("read default sandbox status");
    conpty.write_all(b"/quit\r").expect("write quit command");
    let exit = conpty.wait_for_exit(TIMEOUT).expect("wait for clean exit");
    assert_eq!(
        exit,
        0,
        "Dext exited with {exit}; output:\n{}",
        conpty.text()
    );
    let output = conpty.text();
    assert!(output.contains("Dext"), "missing banner:\n{output}");
    assert!(
        output.contains("\u{1b}[?2004l"),
        "TUI did not disable bracketed paste on exit:\n{output}"
    );
    assert!(
        output.contains("\u{1b}[?25h"),
        "TUI did not restore cursor visibility on exit:\n{output}"
    );
}

#[test]
fn conpty_harness_selfcheck_echoes_through_pseudoconsole() {
    let temp = TempDir::new("dext-conpty-selfcheck").expect("temp directory");
    let comspec = std::env::var_os("COMSPEC")
        .unwrap_or_else(|| OsString::from("C:\\Windows\\System32\\cmd.exe"));
    let command = format!(
        "\"{}\" /c echo conpty-harness-selfcheck-ok",
        Path::new(&comspec).display()
    );
    let environment = environment_block([("NO_COLOR", OsStr::new("1"))]);
    let mut conpty = ConPty::spawn_with(&comspec, &command, &temp.path, environment)
        .expect("spawn cmd in ConPTY");
    let exit = conpty.wait_for_exit(TIMEOUT).expect("wait for cmd exit");
    let output = conpty.text();
    assert_eq!(exit, 0, "cmd exited with {exit}; output:\n{output}");
    assert!(
        output.contains("conpty-harness-selfcheck-ok"),
        "ConPTY harness did not relay child output:\n{output}"
    );
}

#[test]
fn conpty_dext_version_exits_noninteractively() {
    let temp = TempDir::new("dext-conpty-version").expect("temp directory");
    let dext_home = temp.path.join("dext-home");
    std::fs::create_dir_all(&dext_home).expect("dext home");
    let command = format!("\"{}\" --version", env!("CARGO_BIN_EXE_dext"));
    let environment = environment_block([
        ("DEXT_HOME", dext_home.as_os_str()),
        ("HOME", temp.path.as_os_str()),
        ("USERPROFILE", temp.path.as_os_str()),
        ("TEMP", temp.path.as_os_str()),
        ("TMP", temp.path.as_os_str()),
        ("NO_COLOR", OsStr::new("1")),
        ("TERM", OsStr::new("xterm-256color")),
    ]);
    let mut conpty = ConPty::spawn_with(
        OsStr::new(env!("CARGO_BIN_EXE_dext")),
        &command,
        &temp.path,
        environment,
    )
    .expect("spawn dext --version in ConPTY");
    let exit = conpty
        .wait_for_exit(TIMEOUT)
        .expect("wait for dext --version exit");
    let output = conpty.text();
    assert_eq!(
        exit, 0,
        "dext --version exited with {exit}; output:\n{output}"
    );
    assert!(
        output.contains("dext"),
        "dext --version produced no recognizable output:\n{output}"
    );
}

struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn new(label: &str) -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

// Windows kernel handles may be used and closed by another thread; this guard has
// unique ownership and moves into the dedicated output reader.
unsafe impl Send for Handle {}

struct OutputState {
    bytes: Vec<u8>,
    error: Option<String>,
}

struct ConPty {
    input: Handle,
    process: Handle,
    pseudo_console: HPCON,
    output: Arc<Mutex<OutputState>>,
    output_thread: Option<std::thread::JoinHandle<()>>,
}

impl ConPty {
    fn spawn(sandbox: &Path, dext_home: &Path, home: &Path) -> io::Result<Self> {
        let executable = OsString::from(env!("CARGO_BIN_EXE_dext"));
        let command = format!(
            "\"{}\" --no-session --cd \"{}\"",
            env!("CARGO_BIN_EXE_dext"),
            sandbox.display()
        );
        let environment = environment_block([
            ("DEXT_HOME", dext_home.as_os_str()),
            ("HOME", home.as_os_str()),
            ("USERPROFILE", home.as_os_str()),
            ("TEMP", home.as_os_str()),
            ("TMP", home.as_os_str()),
            ("NO_COLOR", OsStr::new("1")),
            ("TERM", OsStr::new("xterm-256color")),
        ]);
        Self::spawn_with(&executable, &command, sandbox, environment)
    }

    fn spawn_with(
        executable: &OsStr,
        command: &str,
        current_dir: &Path,
        mut environment: Vec<u16>,
    ) -> io::Result<Self> {
        unsafe {
            let mut input_read = null_mut();
            let mut input_write = null_mut();
            if CreatePipe(&mut input_read, &mut input_write, null(), 0) == 0 {
                return Err(io::Error::last_os_error());
            }
            let input_read = Handle(input_read);
            let input_write = Handle(input_write);

            let mut output_read = null_mut();
            let mut output_write = null_mut();
            if CreatePipe(&mut output_read, &mut output_write, null(), 0) == 0 {
                return Err(io::Error::last_os_error());
            }
            let output_read = Handle(output_read);
            let output_write = Handle(output_write);

            let mut pseudo_console = 0;
            let result = CreatePseudoConsole(
                COORD { X: COLS, Y: ROWS },
                input_read.0,
                output_write.0,
                0,
                &mut pseudo_console,
            );
            if result < 0 {
                return Err(io::Error::other(format!(
                    "CreatePseudoConsole failed with HRESULT 0x{:08x}",
                    result as u32
                )));
            }

            let mut attribute_size = 0usize;
            InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut attribute_size);
            if attribute_size == 0 {
                ClosePseudoConsole(pseudo_console);
                return Err(io::Error::last_os_error());
            }
            let words = attribute_size.div_ceil(std::mem::size_of::<usize>());
            let mut attributes = vec![0usize; words];
            let attribute_list = attributes.as_mut_ptr().cast();
            if InitializeProcThreadAttributeList(attribute_list, 1, 0, &mut attribute_size) == 0 {
                ClosePseudoConsole(pseudo_console);
                return Err(io::Error::last_os_error());
            }
            // Per the canonical ConPTY sample, lpValue is the HPCON value itself
            // cast as the pointer (CreateProcess consumes it directly), not a
            // pointer to the HPCON as most other proc-thread attributes expect.
            if UpdateProcThreadAttribute(
                attribute_list,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                pseudo_console as *const c_void,
                std::mem::size_of::<HPCON>(),
                null_mut(),
                null(),
            ) == 0
            {
                DeleteProcThreadAttributeList(attribute_list);
                ClosePseudoConsole(pseudo_console);
                return Err(io::Error::last_os_error());
            }

            let mut startup = STARTUPINFOEXW::default();
            startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            startup.lpAttributeList = attribute_list;
            let executable = wide_null(executable);
            let mut command = wide_null(OsStr::new(command));
            let current_dir = wide_null(current_dir.as_os_str());
            let mut process = PROCESS_INFORMATION::default();
            let created = CreateProcessW(
                executable.as_ptr(),
                command.as_mut_ptr(),
                null(),
                null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                environment.as_mut_ptr().cast(),
                current_dir.as_ptr(),
                &startup.StartupInfo,
                &mut process,
            );
            DeleteProcThreadAttributeList(attribute_list);
            if created == 0 {
                ClosePseudoConsole(pseudo_console);
                return Err(io::Error::last_os_error());
            }
            CloseHandle(process.hThread);

            let process = Handle(process.hProcess);
            let output = Arc::new(Mutex::new(OutputState {
                bytes: Vec::new(),
                error: None,
            }));
            let output_thread = match start_output_reader(output_read, output.clone()) {
                Ok(thread) => thread,
                Err(error) => {
                    TerminateProcess(process.0, 1);
                    WaitForSingleObject(process.0, 2_000);
                    ClosePseudoConsole(pseudo_console);
                    return Err(error);
                }
            };

            Ok(Self {
                input: input_write,
                process,
                pseudo_console,
                output,
                output_thread: Some(output_thread),
            })
        }
    }

    fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let mut written = 0u32;
            let chunk = bytes.len().min(u32::MAX as usize) as u32;
            if unsafe {
                WriteFile(
                    self.input.0,
                    bytes.as_ptr(),
                    chunk,
                    &mut written,
                    null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "ConPTY write returned zero",
                ));
            }
            bytes = &bytes[written as usize..];
        }
        Ok(())
    }

    fn wait_for(&self, needle: &str, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut exit_observed: Option<Instant> = None;
        loop {
            if self.visible_text()?.contains(needle) {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            if exit_observed.is_none()
                && unsafe { WaitForSingleObject(self.process.0, 0) } == WAIT_OBJECT_0
            {
                exit_observed = Some(now);
            }
            // After exit, keep draining briefly so late ConPTY output still counts.
            if let Some(exited) = exit_observed
                && now.duration_since(exited) > Duration::from_secs(1)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "did not observe {needle:?}; child {}; visible output:\n{}\nraw output:\n{}",
                self.child_status(),
                self.visible_text()
                    .unwrap_or_else(|error| error.to_string()),
                self.text()
            ),
        ))
    }

    fn child_status(&self) -> String {
        unsafe {
            if WaitForSingleObject(self.process.0, 0) == WAIT_OBJECT_0 {
                let mut code = 0u32;
                if GetExitCodeProcess(self.process.0, &mut code) == 0 {
                    return format!(
                        "exited (exit code unavailable: {})",
                        io::Error::last_os_error()
                    );
                }
                format!("exited with code {code} (0x{code:08x})")
            } else {
                "still running".to_string()
            }
        }
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> io::Result<u32> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            self.output_snapshot()?;
            match unsafe { WaitForSingleObject(self.process.0, 0) } {
                WAIT_OBJECT_0 => {
                    let mut code = 0u32;
                    if unsafe { GetExitCodeProcess(self.process.0, &mut code) } == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    self.close_console_and_join_output()?;
                    self.output_snapshot()?;
                    return Ok(code);
                }
                WAIT_TIMEOUT => std::thread::sleep(Duration::from_millis(20)),
                _ => return Err(io::Error::last_os_error()),
            }
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "Dext did not exit"))
    }

    fn close_console_and_join_output(&mut self) -> io::Result<()> {
        if self.pseudo_console != 0 {
            unsafe {
                ClosePseudoConsole(self.pseudo_console);
            }
            self.pseudo_console = 0;
        }
        if let Some(thread) = self.output_thread.take()
            && thread.join().is_err()
        {
            return Err(io::Error::other("ConPTY output reader panicked"));
        }
        Ok(())
    }

    fn output_snapshot(&self) -> io::Result<Vec<u8>> {
        let output = self
            .output
            .lock()
            .map_err(|_| io::Error::other("ConPTY output reader state was poisoned"))?;
        if let Some(error) = output.error.as_deref() {
            return Err(io::Error::other(error.to_string()));
        }
        Ok(output.bytes.clone())
    }

    fn text(&self) -> String {
        match self.output.lock() {
            Ok(output) => {
                let mut text = String::from_utf8_lossy(&output.bytes).into_owned();
                if let Some(error) = output.error.as_deref() {
                    text.push_str("\n[ConPTY output error: ");
                    text.push_str(error);
                    text.push(']');
                }
                text
            }
            Err(_) => "[ConPTY output reader state was poisoned]".to_string(),
        }
    }

    fn visible_text(&self) -> io::Result<String> {
        Ok(strip_terminal_sequences(&self.output_snapshot()?))
    }
}

impl Drop for ConPty {
    fn drop(&mut self) {
        unsafe {
            if WaitForSingleObject(self.process.0, 0) == WAIT_TIMEOUT {
                TerminateProcess(self.process.0, 1);
                WaitForSingleObject(self.process.0, 2_000);
            }
        }
        let _ = self.close_console_and_join_output();
    }
}

fn start_output_reader(
    output_read: Handle,
    output: Arc<Mutex<OutputState>>,
) -> io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("dext-conpty-output".to_string())
        .spawn(move || {
            // Bind the whole guard so the closure captures (and the thread owns)
            // the Handle rather than a disjoint raw-pointer field capture, which
            // would close the pipe when the spawning scope returns.
            let output_read = output_read;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let mut read = 0u32;
                if unsafe {
                    ReadFile(
                        output_read.0,
                        buffer.as_mut_ptr(),
                        buffer.len() as u32,
                        &mut read,
                        null_mut(),
                    )
                } == 0
                {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(ERROR_BROKEN_PIPE as i32)
                        && let Ok(mut output) = output.lock()
                        && output.error.is_none()
                    {
                        output.error = Some(format!("ConPTY output read failed: {error}"));
                    }
                    return;
                }
                if read == 0 {
                    return;
                }
                let read = read as usize;
                let Ok(mut output) = output.lock() else {
                    return;
                };
                if output.bytes.len().saturating_add(read) > OUTPUT_CAP {
                    if output.error.is_none() {
                        output.error = Some(format!(
                            "ConPTY output exceeded the {OUTPUT_CAP} byte test limit"
                        ));
                    }
                    drop(output);
                    continue;
                }
                output.bytes.extend_from_slice(&buffer[..read]);
            }
        })
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

fn environment_block<const N: usize>(overrides: [(&str, &OsStr); N]) -> Vec<u16> {
    let inherited = ["PATH", "PATHEXT", "SystemRoot", "WINDIR", "COMSPEC"];
    let override_names = overrides.map(|(name, _)| name);
    let mut values = std::env::vars_os()
        .filter(|(name, _)| {
            inherited
                .iter()
                .any(|inherited| name.to_string_lossy().eq_ignore_ascii_case(inherited))
                && !override_names
                    .iter()
                    .any(|overridden| name.to_string_lossy().eq_ignore_ascii_case(overridden))
        })
        .collect::<Vec<_>>();
    values.extend(
        overrides
            .into_iter()
            .map(|(name, value)| (OsString::from(name), value.to_os_string())),
    );
    values.sort_by_cached_key(|(name, _)| name.to_string_lossy().to_uppercase());
    let mut block = Vec::new();
    for (name, value) in values {
        let mut entry = name.encode_wide().collect::<Vec<_>>();
        entry.push(u16::from(b'='));
        entry.extend(value.encode_wide());
        entry.push(0);
        block.extend(entry);
    }
    block.push(0);
    block
}

fn strip_terminal_sequences(bytes: &[u8]) -> String {
    let mut visible = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != 0x1b {
            visible.push(bytes[index]);
            index += 1;
            continue;
        }
        index += 1;
        match bytes.get(index).copied() {
            Some(b'[') => {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            Some(b']') => {
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == 0x07 {
                        index += 1;
                        break;
                    }
                    if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
                        index += 2;
                        break;
                    }
                    index += 1;
                }
            }
            Some(_) => index += 1,
            None => {}
        }
    }
    String::from_utf8_lossy(&visible).into_owned()
}
