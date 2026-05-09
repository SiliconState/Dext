#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TUI_COLS: u16 = 120;
const TUI_ROWS: u16 = 40;

#[test]
fn tui_smoke_launches_real_binary_in_pty() {
    let temp = TempDir::new("dext-tui-smoke").expect("temp dir");
    let sandbox = temp.path().join("sandbox");
    let dext_home = temp.path().join("dext-home");
    let home = temp.path().join("home");
    fs::create_dir_all(&sandbox).expect("sandbox");
    fs::create_dir_all(&dext_home).expect("dext home");
    fs::create_dir_all(&home).expect("home");

    let mut pty = Pty::open(TUI_COLS, TUI_ROWS).expect("open pty");
    let mut child = spawn_dext(&pty, &sandbox, &dext_home, &home).expect("spawn dext in pty");

    assert_visible(&mut pty, &mut child, "Dext v", Duration::from_secs(5));
    assert_visible(&mut pty, &mut child, "sandbox", Duration::from_secs(2));
    assert_visible(&mut pty, &mut child, "model", Duration::from_secs(2));
    assert_visible(&mut pty, &mut child, "Ctrl+D quit", Duration::from_secs(2));

    pty.write_all_retry(b"?").expect("send help key");
    assert_visible(&mut pty, &mut child, "keymap", Duration::from_secs(3));
    assert_visible(&mut pty, &mut child, "Ctrl+O", Duration::from_secs(2));
    assert_visible(
        &mut pty,
        &mut child,
        "insertnewline",
        Duration::from_secs(2),
    );

    pty.write_all_retry(&[0x04]).expect("send Ctrl+D");
    let status = wait_for_exit(&mut child, Duration::from_secs(5), || pty.visible_text())
        .expect("wait for dext exit");
    pty.read_available().expect("final pty drain");
    let visible = pty.visible_text();

    assert!(
        status.success(),
        "dext exited with {status}; visible tail:\n{}",
        tail(&visible, 3000)
    );
    assert_no_crash_text(&visible);
}

fn spawn_dext(pty: &Pty, sandbox: &Path, dext_home: &Path, home: &Path) -> io::Result<Child> {
    let slave = pty.slave_fd();
    let stdin = unsafe { File::from_raw_fd(dup_fd(slave)?) };
    let stdout = unsafe { File::from_raw_fd(dup_fd(slave)?) };
    let stderr = unsafe { File::from_raw_fd(dup_fd(slave)?) };
    let path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dext"));
    cmd.args(["--no-session", "--cd"])
        .arg(sandbox)
        .current_dir(sandbox)
        .env_clear()
        .env("PATH", path)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env("LANG", "C.UTF-8")
        .env("HOME", home)
        .env("DEXT_HOME", dext_home)
        .env("DEXT_APPROVAL", "never")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(slave, libc::TIOCSCTTY, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.spawn()
}

fn assert_visible(pty: &mut Pty, child: &mut Child, needle: &str, timeout: Duration) {
    let found = pty
        .wait_for(child, needle, timeout)
        .unwrap_or_else(|err| panic!("waiting for {needle:?} failed: {err}"));
    if !found {
        let visible = pty.visible_text();
        panic!(
            "did not see {needle:?} within {timeout:?}; visible tail:\n{}",
            tail(&visible, 3000)
        );
    }
}

fn wait_for_exit(
    child: &mut Child,
    timeout: Duration,
    capture: impl FnOnce() -> String,
) -> io::Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let visible = capture();
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "dext did not exit within {timeout:?}; visible tail:\n{}",
                tail(&visible, 3000)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn assert_no_crash_text(visible: &str) {
    for needle in [
        "panicked at",
        "thread 'main' panicked",
        "[dext crash snapshot",
        "[error]",
    ] {
        assert!(
            !visible.contains(needle),
            "unexpected crash/error marker {needle:?}; visible tail:\n{}",
            tail(visible, 3000)
        );
    }
}

struct Pty {
    master: File,
    slave: RawFd,
    capture: Vec<u8>,
}

impl Pty {
    fn open(cols: u16, rows: u16) -> io::Result<Self> {
        let mut master = -1;
        let mut slave = -1;
        let winsize = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &winsize,
            )
        };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        set_nonblocking(master)?;
        let master = unsafe { File::from_raw_fd(master) };
        Ok(Self {
            master,
            slave,
            capture: Vec::new(),
        })
    }

    fn slave_fd(&self) -> RawFd {
        self.slave
    }

    fn read_available(&mut self) -> io::Result<()> {
        let mut buf = [0u8; 4096];
        loop {
            match self.master.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => self.capture.extend_from_slice(&buf[..n]),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(err) if err.raw_os_error() == Some(libc::EIO) => return Ok(()),
                Err(err) => return Err(err),
            }
        }
    }

    fn wait_for(&mut self, child: &mut Child, needle: &str, timeout: Duration) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            self.read_available()?;
            self.answer_cursor_position_queries()?;
            if self.visible_text().contains(needle) {
                return Ok(true);
            }
            if child.try_wait()?.is_some() {
                self.read_available()?;
                return Ok(self.visible_text().contains(needle));
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn answer_cursor_position_queries(&mut self) -> io::Result<()> {
        let query = b"\x1b[6n";
        let mut cleaned = Vec::with_capacity(self.capture.len());
        let mut replies = 0usize;
        let mut i = 0usize;
        while i < self.capture.len() {
            if self.capture[i..].starts_with(query) {
                replies += 1;
                i += query.len();
            } else {
                cleaned.push(self.capture[i]);
                i += 1;
            }
        }
        self.capture = cleaned;
        for _ in 0..replies {
            self.write_all_retry(b"\x1b[1;1R")?;
        }
        Ok(())
    }

    fn write_all_retry(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < bytes.len() {
            match self.master.write(&bytes[written..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "pty write returned 0",
                    ));
                }
                Ok(n) => written += n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => return Err(err),
            }
        }
        self.master.flush()
    }

    fn visible_text(&self) -> String {
        strip_ansi(&String::from_utf8_lossy(&self.capture))
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.slave);
        }
    }
}

fn dup_fd(fd: RawFd) -> io::Result<RawFd> {
    let duped = unsafe { libc::dup(fd) };
    if duped == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(duped)
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    let mut prev_esc = false;
                    for c in chars.by_ref() {
                        if c == '\u{7}' || (prev_esc && c == '\\') {
                            break;
                        }
                        prev_esc = c == '\u{1b}';
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        if ch == '\r' || ch == '\n' || ch == '\t' || !ch.is_control() {
            out.push(ch);
        }
    }
    out
}

fn tail(s: &str, max_chars: usize) -> String {
    let mut chars: Vec<char> = s.chars().rev().take(max_chars).collect();
    chars.reverse();
    chars.into_iter().collect()
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> io::Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
