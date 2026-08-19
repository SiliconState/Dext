#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TUI_COLS: u16 = 120;
const TUI_ROWS: u16 = 40;
const TUI_NARROW_COLS: u16 = 80;
const TUI_NARROW_ROWS: u16 = 24;
const TUI_WIDE_COLS: u16 = 180;
const TUI_WIDE_ROWS: u16 = 40;

#[test]
fn tui_smoke_launches_real_binary_in_pty() {
    run_tui_smoke(TUI_COLS, TUI_ROWS, true);
}

#[test]
fn tui_smoke_launches_narrow_and_wide_terminals() {
    run_tui_smoke(TUI_NARROW_COLS, TUI_NARROW_ROWS, false);
    run_tui_smoke(TUI_WIDE_COLS, TUI_WIDE_ROWS, false);
}

#[test]
fn tui_resize_keeps_inline_session_responsive_and_dsr_bounded() {
    let temp = TempDir::new("dext-tui-resize").expect("temp dir");
    let sandbox = temp.path().join("sandbox");
    let dext_home = temp.path().join("dext-home");
    let home = temp.path().join("home");
    fs::create_dir_all(&sandbox).expect("sandbox");
    fs::create_dir_all(&dext_home).expect("dext home");
    fs::create_dir_all(&home).expect("home");

    let (mock_base_url, release_stream, mock_server) = spawn_slow_openai_server();
    let mut pty = Pty::open(TUI_COLS, TUI_ROWS).expect("open pty");
    let mut child = spawn_dext_with_env(
        &pty,
        &sandbox,
        &dext_home,
        &home,
        &[
            ("DEXT_PROVIDER", "local"),
            ("DEXT_BASE_URL", mock_base_url.as_str()),
            ("DEXT_MODEL", "mock-model"),
            ("DEXT_MODEL_FORCE", "1"),
        ],
    )
    .expect("spawn dext in pty");
    assert_visible(&mut pty, &mut child, "◆ Dext  v", Duration::from_secs(5));

    const TRANSCRIPT_BLOCKS: usize = 24;
    // Each submitted command contributes a separator, a three-row user card,
    // and one rendered slash-result row. The streaming prompt contributes the
    // same separator/card rows before the resize begins.
    const ROWS_PER_SUBMITTED_COMMAND: usize = 5;
    const STREAMING_PROMPT_ROWS: usize = 4;
    const BANNER_ROW_BUDGET: usize = 12;
    for expected in 1..=TRANSCRIPT_BLOCKS {
        let command = format!("/compact {expected}%\r");
        let output = format!("compact threshold set to {expected}%");
        pty.write_all_retry(command.as_bytes())
            .expect("send fixture command");
        let found = pty
            .wait_for(&mut child, &output, Duration::from_secs(5))
            .expect("wait for fixture output");
        assert!(
            found,
            "fixture block {expected} did not render; visible tail:\n{}",
            tail(&pty.visible_text(), 3000)
        );
    }

    pty.write_all_retry(b"Answer exactly: streaming-first streaming-last\r")
        .expect("start streaming fixture");
    assert_visible(
        &mut pty,
        &mut child,
        "streaming-first",
        Duration::from_secs(5),
    );
    pty.write_all_retry(b"/compact 7")
        .expect("type while stream is live");

    pty.pump_for(&mut child, Duration::from_millis(160))
        .expect("settle populated transcript");
    let before_resize = pty.terminal_io_counts();
    let before_resize_capture = pty.capture.len();

    let resize_burst = [
        (100, 32),
        (92, 30),
        (84, 27),
        (TUI_NARROW_COLS, TUI_NARROW_ROWS),
    ];
    for (index, (cols, rows)) in resize_burst.into_iter().enumerate() {
        pty.resize(&child, cols, rows).expect("resize burst step");
        if index == 1 {
            pty.write_all_retry(b"%")
                .expect("continue typing during resize burst");
        }
        pty.pump_for(&mut child, Duration::from_millis(25))
            .expect("pump resize burst step");
    }
    pty.pump_for(&mut child, Duration::from_millis(500))
        .expect("settle narrow resize burst");
    let narrow_resize = pty.terminal_io_counts() - before_resize;
    eprintln!("narrow resize terminal I/O: {narrow_resize:?}");
    assert!(
        narrow_resize.dsr_queries <= resize_burst.len().saturating_add(1),
        "resize burst issued more than one cursor query per OS resize: {narrow_resize:?}"
    );
    assert!(
        narrow_resize.clear_all > 0 && narrow_resize.clear_all <= resize_burst.len(),
        "resize burst must rebuild at least once and at most once per effective width: {narrow_resize:?}"
    );
    assert_eq!(
        narrow_resize.purge_scrollback, narrow_resize.clear_all,
        "every full replay must pair one scrollback purge with one display clear: {narrow_resize:?}"
    );
    assert_resize_resets_clear_before_purge_and_replay_one_banner(
        &pty.capture[before_resize_capture..],
        narrow_resize.clear_all,
    );
    let rendered_row_budget = TRANSCRIPT_BLOCKS
        .saturating_mul(ROWS_PER_SUBMITTED_COMMAND)
        .saturating_add(STREAMING_PROMPT_ROWS)
        .saturating_add(BANNER_ROW_BUDGET);
    let narrow_clear_bound = resize_clear_after_bound(
        rendered_row_budget,
        resize_burst.len(),
        usize::from(TUI_NARROW_ROWS),
        resize_burst.len(),
    );
    assert!(
        narrow_resize.clear_after_cursor <= narrow_clear_bound,
        "narrow resize exceeded its per-width, terminal-height chunk bound ({narrow_clear_bound}): {narrow_resize:?}"
    );

    let before_wide = pty.terminal_io_counts();
    let before_wide_capture = pty.capture.len();
    pty.resize(&child, TUI_WIDE_COLS, TUI_WIDE_ROWS)
        .expect("resize wide");
    pty.pump_for(&mut child, Duration::from_millis(500))
        .expect("settle wide resize");
    let wide_resize = pty.terminal_io_counts() - before_wide;
    eprintln!("wide resize terminal I/O: {wide_resize:?}");
    assert!(
        wide_resize.dsr_queries <= 3,
        "wide resize issued transcript-proportional cursor queries: {wide_resize:?}"
    );
    assert_eq!(
        wide_resize.clear_all, 1,
        "one effective wide resize must clear the visible display once before full replay: {wide_resize:?}"
    );
    assert_eq!(
        wide_resize.purge_scrollback, wide_resize.clear_all,
        "the wide replay must pair its scrollback purge and display clear: {wide_resize:?}"
    );
    assert_resize_resets_clear_before_purge_and_replay_one_banner(
        &pty.capture[before_wide_capture..],
        wide_resize.clear_all,
    );
    let wide_clear_bound =
        resize_clear_after_bound(rendered_row_budget, 1, usize::from(TUI_WIDE_ROWS), 1);
    assert!(
        wide_resize.clear_after_cursor <= wide_clear_bound,
        "wide resize exceeded its single-replay, terminal-height chunk bound ({wide_clear_bound}): {wide_resize:?}"
    );

    assert!(
        child.try_wait().expect("query child status").is_none(),
        "TUI exited during resize; visible tail:\n{}",
        tail(&pty.visible_text(), 3000)
    );
    release_stream.send(()).expect("release mock stream");
    assert_visible(
        &mut pty,
        &mut child,
        "streaming-final",
        Duration::from_secs(10),
    );
    pty.pump_for(&mut child, Duration::from_millis(800))
        .expect("settle completed streaming turn");
    pty.write_all_retry(b"\r")
        .expect("submit input assembled during resize");
    assert_visible(
        &mut pty,
        &mut child,
        "compact threshold set to 7%",
        Duration::from_secs(3),
    );
    assert!(
        child.try_wait().expect("query child status").is_none(),
        "TUI exited during resize; visible tail:\n{}",
        tail(&pty.visible_text(), 3000)
    );

    pty.write_all_retry(b"\x04").expect("send raw Ctrl+D");
    let status =
        wait_for_exit(&mut child, Duration::from_secs(5), &mut pty).expect("wait for dext exit");
    assert!(
        status.success(),
        "visible tail:\n{}",
        tail(&pty.visible_text(), 3000)
    );
    assert_no_crash_text(&pty.visible_text());
    mock_server.join().expect("mock server thread");
}

#[test]
fn tui_smoke_shift_enter_inserts_newline() {
    let temp = TempDir::new("dext-tui-shift-enter").expect("temp dir");
    let sandbox = temp.path().join("sandbox");
    let dext_home = temp.path().join("dext-home");
    let home = temp.path().join("home");
    fs::create_dir_all(&sandbox).expect("sandbox");
    fs::create_dir_all(&dext_home).expect("dext home");
    fs::create_dir_all(&home).expect("home");

    let mut pty = Pty::open(TUI_COLS, TUI_ROWS).expect("open pty");
    let mut child = spawn_dext_with_env(
        &pty,
        &sandbox,
        &dext_home,
        &home,
        &[("TERM_PROGRAM", "WezTerm")],
    )
    .expect("spawn dext in pty");

    assert_visible(&mut pty, &mut child, "◆ Dext  v", Duration::from_secs(5));
    pty.write_all_retry(b"hello").expect("send text");
    pty.write_all_retry(b"\x1b[13;2u")
        .expect("send Shift+Enter CSI-u");
    pty.write_all_retry(b"world").expect("send more text");
    assert_visible(&mut pty, &mut child, "hello", Duration::from_secs(2));
    assert_visible(&mut pty, &mut child, "world", Duration::from_secs(2));
    let visible = pty.visible_text();
    assert!(
        visible.contains("│hello") && visible.contains("│     world"),
        "{}",
        tail(&visible, 3000)
    );

    pty.write_all_retry(b"\x1b[100;5u")
        .expect("send Ctrl+D CSI-u");
    let status =
        wait_for_exit(&mut child, Duration::from_secs(5), &mut pty).expect("wait for dext exit");
    assert!(
        status.success(),
        "visible tail:\n{}",
        tail(&pty.visible_text(), 3000)
    );
}

fn run_tui_smoke(cols: u16, rows: u16, exercise_help: bool) {
    let temp = TempDir::new("dext-tui-smoke").expect("temp dir");
    let sandbox = temp.path().join("sandbox");
    let dext_home = temp.path().join("dext-home");
    let home = temp.path().join("home");
    fs::create_dir_all(&sandbox).expect("sandbox");
    fs::create_dir_all(&dext_home).expect("dext home");
    fs::create_dir_all(&home).expect("home");

    let mut pty = Pty::open(cols, rows).expect("open pty");
    let mut child = spawn_dext(&pty, &sandbox, &dext_home, &home).expect("spawn dext in pty");

    assert_visible(&mut pty, &mut child, "◆ Dext  v", Duration::from_secs(5));
    assert_visible(&mut pty, &mut child, "Model", Duration::from_secs(2));
    assert_visible(&mut pty, &mut child, "Approval", Duration::from_secs(2));
    assert_visible(&mut pty, &mut child, "Tip", Duration::from_secs(2));
    assert_visible(
        &mut pty,
        &mut child,
        "Type a request…   @ files · / commands",
        Duration::from_secs(2),
    );

    if exercise_help {
        pty.write_all_retry(b"?").expect("send help key");
        assert_visible(&mut pty, &mut child, "keymap", Duration::from_secs(3));
        assert_visible(&mut pty, &mut child, "Ctrl+O", Duration::from_secs(2));
        assert_visible(&mut pty, &mut child, "Ctrl+T", Duration::from_secs(2));
        assert_visible(
            &mut pty,
            &mut child,
            "insertnewline",
            Duration::from_secs(2),
        );
    }

    pty.write_all_retry(b"\x04").expect("send raw Ctrl+D");
    let status =
        wait_for_exit(&mut child, Duration::from_secs(5), &mut pty).expect("wait for dext exit");
    pty.read_available().expect("final pty drain");
    let visible = pty.visible_text();

    assert!(
        status.success(),
        "dext exited with {status}; visible tail:\n{}",
        tail(&visible, 3000)
    );
    assert_no_crash_text(&visible);
    assert!(
        !visible.contains("+12 chars"),
        "stale truncation/debug artifact visible at {cols}x{rows}:\n{}",
        tail(&visible, 3000)
    );
}

fn spawn_slow_openai_server() -> (
    String,
    std::sync::mpsc::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock OpenAI server");
    let address = listener.local_addr().expect("mock server address");
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut saw_chat_request = false;
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                break;
            };
            if serve_mock_openai_request(stream, &release_rx) {
                saw_chat_request = true;
                break;
            }
        }
        assert!(saw_chat_request, "mock server never received chat request");
    });
    (format!("http://{address}"), release_tx, server)
}

fn serve_mock_openai_request(
    mut stream: TcpStream,
    release: &std::sync::mpsc::Receiver<()>,
) -> bool {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set mock request timeout");
    let mut request = Vec::new();
    let mut buf = [0u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buf).expect("read mock request headers");
        assert!(read > 0, "client closed before mock request headers");
        request.extend_from_slice(&buf[..read]);
    }
    let content_length = String::from_utf8_lossy(&request)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let body_start = request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .unwrap_or(request.len());
    while request.len().saturating_sub(body_start) < content_length {
        let read = stream.read(&mut buf).expect("read mock request body");
        assert!(read > 0, "client closed before mock request body");
        request.extend_from_slice(&buf[..read]);
    }

    if !String::from_utf8_lossy(&request).starts_with("POST /v1/chat/completions ") {
        stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("write mock discovery response");
        return false;
    }

    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        )
        .expect("write mock response headers");
    let first_frame = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}},\"finish_reason\":null}}]}}\n\n",
        serde_json::to_string("streaming-first").expect("encode mock delta")
    );
    stream
        .write_all(first_frame.as_bytes())
        .expect("write first stream delta");
    stream.flush().expect("flush first stream delta");
    release
        .recv_timeout(Duration::from_secs(10))
        .expect("test did not release mock stream");
    stream
        .write_all(
            b"data: {\"choices\":[{\"delta\":{\"content\":\" streaming-last streaming-final verified output remains responsive after the populated-history resize burst and preserves editable input\"},\"finish_reason\":null}]}\n\n",
        )
        .expect("write delayed stream delta");
    stream
        .write_all(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        )
        .expect("finish mock stream");
    stream.flush().expect("flush final stream delta");
    true
}

fn spawn_dext(pty: &Pty, sandbox: &Path, dext_home: &Path, home: &Path) -> io::Result<Child> {
    spawn_dext_with_env(pty, sandbox, dext_home, home, &[])
}

fn spawn_dext_with_env(
    pty: &Pty,
    sandbox: &Path,
    dext_home: &Path,
    home: &Path,
    extra_env: &[(&str, &str)],
) -> io::Result<Child> {
    let stdin = unsafe { File::from_raw_fd(dup_fd(pty.slave_file())?) };
    let stdout = unsafe { File::from_raw_fd(dup_fd(pty.slave_file())?) };
    let stderr = unsafe { File::from_raw_fd(dup_fd(pty.slave_file())?) };
    let path = std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"));

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dext"));
    cmd.args(["--no-session", "--cd"])
        .arg(sandbox)
        .current_dir(sandbox)
        .env_clear()
        .env("PATH", path)
        .env("TERM", "xterm-256color")
        .env("TERM_PROGRAM", "Apple_Terminal")
        .env("COLORTERM", "truecolor")
        .env("LANG", "C")
        .env("HOME", home)
        .env("DEXT_HOME", dext_home)
        .env("DEXT_APPROVAL", "never")
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    unsafe {
        cmd.pre_exec(|| {
            // A real controlling PTY is required for macOS to deliver resize state consistently.
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as libc::c_ulong, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.spawn()
}

fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        let pid = child.id() as libc::pid_t;
        let _ = libc::kill(-pid, libc::SIGTERM);
        std::thread::sleep(Duration::from_millis(50));
        let _ = libc::kill(-pid, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn assert_visible(pty: &mut Pty, child: &mut Child, needle: &str, timeout: Duration) {
    let found = pty
        .wait_for(child, needle, timeout)
        .unwrap_or_else(|err| panic!("waiting for {needle:?} failed: {err}"));
    if !found {
        let visible = pty.visible_text();
        terminate_child(child);
        panic!(
            "did not see {needle:?} within {timeout:?}; visible tail:\n{}",
            tail(&visible, 3000)
        );
    }
}

fn wait_for_exit(
    child: &mut Child,
    timeout: Duration,
    pty: &mut Pty,
) -> io::Result<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        pty.read_available()?;
        pty.answer_cursor_position_queries()?;
        if let Some(status) = child.try_wait()? {
            pty.read_available()?;
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let visible = pty.visible_text();
            terminate_child(child);
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

fn byte_offsets(haystack: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() {
        return Vec::new();
    }
    haystack
        .windows(needle.len())
        .enumerate()
        .filter_map(|(index, window)| (window == needle).then_some(index))
        .collect()
}

fn assert_resize_resets_clear_before_purge_and_replay_one_banner(
    capture: &[u8],
    expected_resets: usize,
) {
    let clears = byte_offsets(capture, b"\x1b[2J");
    let purges = byte_offsets(capture, b"\x1b[3J");
    assert_eq!(clears.len(), expected_resets, "unexpected display clears");
    assert_eq!(
        purges.len(),
        expected_resets,
        "unexpected scrollback purges"
    );

    for index in 0..expected_resets {
        assert!(
            clears[index] < purges[index] && (index == 0 || purges[index - 1] < clears[index]),
            "reset {index} did not clear the visible display before purging scrollback"
        );
        let segment_end = clears.get(index + 1).copied().unwrap_or(capture.len());
        let replay = strip_ansi(&String::from_utf8_lossy(
            &capture[purges[index] + b"\x1b[3J".len()..segment_end],
        ));
        assert_eq!(
            replay.matches("◆ Dext  v").count(),
            1,
            "reset {index} did not replay exactly one Dext intro: {replay:?}"
        );
    }
}

fn resize_clear_after_bound(
    rendered_row_budget: usize,
    max_replay_count: usize,
    chunk_rows: usize,
    resize_events: usize,
) -> usize {
    let item_boundary_chunks = rendered_row_budget
        .div_ceil(chunk_rows.max(1))
        .saturating_add(1);
    max_replay_count
        .saturating_mul(item_boundary_chunks.saturating_add(1))
        .saturating_add(resize_events)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TerminalIoCounts {
    dsr_queries: usize,
    clear_all: usize,
    purge_scrollback: usize,
    clear_after_cursor: usize,
    clear_current_line: usize,
}

impl std::ops::Sub for TerminalIoCounts {
    type Output = Self;

    fn sub(self, earlier: Self) -> Self {
        Self {
            dsr_queries: self.dsr_queries.saturating_sub(earlier.dsr_queries),
            clear_all: self.clear_all.saturating_sub(earlier.clear_all),
            purge_scrollback: self
                .purge_scrollback
                .saturating_sub(earlier.purge_scrollback),
            clear_after_cursor: self
                .clear_after_cursor
                .saturating_sub(earlier.clear_after_cursor),
            clear_current_line: self
                .clear_current_line
                .saturating_sub(earlier.clear_current_line),
        }
    }
}

struct Pty {
    master: File,
    slave: File,
    capture: Vec<u8>,
    dsr_queries: usize,
}

impl Pty {
    fn open(cols: u16, rows: u16) -> io::Result<Self> {
        let mut master = -1;
        let mut slave = -1;
        #[cfg(target_os = "macos")]
        let mut winsize = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        #[cfg(not(target_os = "macos"))]
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
                std::ptr::null_mut(),
                {
                    #[cfg(target_os = "macos")]
                    {
                        &mut winsize
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        &winsize
                    }
                },
            )
        };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        set_nonblocking(master)?;
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        Ok(Self {
            master,
            slave,
            capture: Vec::new(),
            dsr_queries: 0,
        })
    }

    fn slave_file(&self) -> &File {
        &self.slave
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

    fn pump_for(&mut self, child: &mut Child, duration: Duration) -> io::Result<()> {
        let deadline = Instant::now() + duration;
        loop {
            self.read_available()?;
            self.answer_cursor_position_queries()?;
            if let Some(status) = child.try_wait()? {
                return Err(io::Error::other(format!(
                    "dext exited while pumping PTY: {status}; visible tail: {}",
                    tail(&self.visible_text(), 1000)
                )));
            }
            if Instant::now() >= deadline {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn resize(&self, child: &Child, cols: u16, rows: u16) -> io::Result<()> {
        let winsize = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let rc = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &winsize) };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        let rc = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGWINCH) };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn terminal_io_counts(&self) -> TerminalIoCounts {
        TerminalIoCounts {
            dsr_queries: self.dsr_queries,
            clear_all: count_bytes(&self.capture, b"\x1b[2J"),
            purge_scrollback: count_bytes(&self.capture, b"\x1b[3J"),
            clear_after_cursor: count_bytes(&self.capture, b"\x1b[J")
                + count_bytes(&self.capture, b"\x1b[0J"),
            clear_current_line: count_bytes(&self.capture, b"\x1b[2K"),
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
        self.dsr_queries = self.dsr_queries.saturating_add(replies);
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

fn dup_fd(file: &File) -> io::Result<std::os::fd::RawFd> {
    let duped = unsafe { libc::dup(file.as_raw_fd()) };
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

fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
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
