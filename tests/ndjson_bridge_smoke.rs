// Real-binary smoke for `--input ndjson`: the host protocol's lifecycle and
// stdout-purity guarantees, exercised over an actual duplex pipe. No provider
// is needed — nothing here starts a turn.
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn spawn(root: &std::path::Path, extra: &[&str]) -> Child {
    let path = std::env::var_os("PATH").unwrap_or_default();
    Command::new(env!("CARGO_BIN_EXE_dext"))
        .args([
            "--input",
            "ndjson",
            "--output",
            "stream-json",
            "--no-session",
            "--cd",
        ])
        .arg(root)
        .args(extra)
        .current_dir(root)
        .env_clear()
        .env("PATH", path)
        .env("HOME", root)
        .env("DEXT_HOME", root.join(".dext"))
        .env("DEXT_APPROVAL", "never")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn dext")
}

/// Every stdout line must be one JSON object; returns them parsed.
fn drain_json(child: &mut Child) -> Vec<serde_json::Value> {
    let out = child.stdout.take().expect("stdout");
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let lines = BufReader::new(out).lines().collect::<Result<Vec<_>, _>>();
        let _ = tx.send(lines);
    });
    let lines = rx
        .recv_timeout(Duration::from_secs(20))
        .unwrap_or_else(|error| {
            let _ = child.kill();
            let _ = child.wait();
            panic!("stdout did not close within deadline: {error}");
        })
        .expect("read stdout");
    reader.join().expect("stdout reader");
    lines
        .into_iter()
        .filter(|l| !l.is_empty() || panic!("blank line on stdout breaks NDJSON"))
        .map(|l| {
            serde_json::from_str(&l).unwrap_or_else(|e| panic!("non-JSON stdout line {l:?}: {e}"))
        })
        .collect()
}

fn wait_within(child: &mut Child, limit: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("dext did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn temp_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("dext-ndjson-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn ready_first_then_close_terminates_with_pure_json_stdout() {
    let root = temp_root("close");
    let mut child = spawn(&root, &[]);
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, r#"{{"type":"bogus","seq":7}}"#).unwrap();
    writeln!(stdin, r#"{{"type":"close","seq":8}}"#).unwrap();
    let events = drain_json(&mut child);
    wait_within(&mut child, Duration::from_secs(20));
    assert_eq!(events[0]["event"], "ready", "{events:?}");
    assert_eq!(events[0]["data"]["input"], "ndjson");
    assert_eq!(events[0]["data"]["ui_protocol"], 1);
    assert!(
        events[0]["data"]["frames"]
            .as_array()
            .unwrap()
            .iter()
            .any(|frame| frame == "ui.response")
    );
    let acks: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "input_ack")
        .collect();
    assert_eq!(acks[0]["data"]["route"], "invalid");
    assert_eq!(acks[0]["data"]["seq"], 7);
    assert_eq!(acks[1]["data"]["route"], "close");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn background_compaction_defaults_on_and_session_controls_remain_provider_free() {
    let root = temp_root("background-setting-controls");
    let mut child = spawn(&root, &[]);
    let mut stdin = child.stdin.take().unwrap();
    for command in [
        "/compact background off",
        "/compact background status",
        "/compact background on",
    ] {
        writeln!(
            stdin,
            "{}",
            serde_json::json!({"type":"control","command":command})
        )
        .unwrap();
    }
    writeln!(stdin, r#"{{"type":"close"}}"#).unwrap();
    drop(stdin);
    let events = drain_json(&mut child);
    assert!(wait_within(&mut child, Duration::from_secs(20)).success());
    let ready = events
        .iter()
        .find(|event| event["event"] == "ready")
        .unwrap();
    assert_eq!(ready["data"]["background_compact"], true);
    let settings = events
        .iter()
        .filter(|event| event["event"] == "background_compaction_setting")
        .map(|event| event["data"]["enabled"].as_bool().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(settings, [false, false, true]);
    assert!(
        !events
            .iter()
            .any(|event| event["event"] == "turn_start" || event["event"] == "compact_start")
    );
    let mut disabled = spawn(&root, &["--background-compact=off"]);
    writeln!(disabled.stdin.as_mut().unwrap(), r#"{{"type":"close"}}"#).unwrap();
    let events = drain_json(&mut disabled);
    assert!(wait_within(&mut disabled, Duration::from_secs(20)).success());
    assert_eq!(events[0]["data"]["background_compact"], false);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn background_compaction_preference_survives_provider_free_restart_and_cli_override() {
    let root = temp_root("background-setting-persist");
    let invoke = |extra: &[&str], controls: &[&str]| {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut child = Command::new(env!("CARGO_BIN_EXE_dext"))
            .args(["--input", "ndjson", "--output", "stream-json", "--cd"])
            .arg(&root)
            .args(extra)
            .current_dir(&root)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &root)
            .env("DEXT_HOME", root.join(".dext"))
            .env("DEXT_PROVIDER", "local")
            .env("DEXT_BASE_URL", "http://127.0.0.1:1")
            .env("DEXT_MODEL", "mock-model")
            .env("DEXT_MODEL_FORCE", "1")
            .env("DEXT_APPROVAL", "never")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        for command in controls {
            writeln!(
                input,
                "{}",
                serde_json::json!({"type":"control", "command": command})
            )
            .unwrap();
        }
        writeln!(input, r#"{{"type":"close"}}"#).unwrap();
        drop(input);
        let events = drain_json(&mut child);
        let success = wait_within(&mut child, Duration::from_secs(20)).success();
        assert!(success, "{events:?}");
        let ready = events
            .iter()
            .find(|event| event["event"] == "ready")
            .unwrap()
            .clone();
        assert!(!events.iter().any(|event| event["event"] == "turn_start"));
        ready
    };
    let first = invoke(&[], &["/compact background off"]);
    assert_eq!(first["data"]["background_compact"], true);
    let session_id = first["data"]["session_id"].as_str().unwrap();
    let projects = root.join(".dext/projects");
    let project = std::fs::read_dir(&projects)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let saved = project
        .join("sessions")
        .join(session_id)
        .join("_latest.jsonl");
    assert!(
        saved.is_file(),
        "an empty session must save its per-session preference"
    );
    let header: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(&saved)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(header["background_compact"], false);
    let resume = format!("--resume={}", saved.display());
    let second = invoke(&[&resume], &[]);
    assert_eq!(second["data"]["background_compact"], false);
    assert_eq!(second["data"]["session_id"], first["data"]["session_id"]);
    let overridden = invoke(&[&resume, "--background-compact=on"], &[]);
    assert_eq!(overridden["data"]["background_compact"], true);
    let third = invoke(&[&resume], &[]);
    assert_eq!(third["data"]["background_compact"], true);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn incompatible_flags_fail_without_reading_stdin() {
    for extra in [&["-p"][..], &["--pack", "missing-pack"][..]] {
        let root = temp_root(if extra[0] == "-p" { "print" } else { "pack" });
        let mut child = spawn(&root, extra);
        assert!(!wait_within(&mut child, Duration::from_secs(20)).success());
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn fork_slash_and_close_keep_stdout_json() {
    let root = temp_root("fork");
    std::fs::write(
        root.join("resume.jsonl"),
        "{\"version\":4,\"model\":\"test-model\",\"system\":\"test\"}\n",
    )
    .unwrap();
    let mut child = spawn(&root, &["--fork", "--resume=resume.jsonl"]);
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, r#"{{"type":"control","command":"/effort status"}}"#).unwrap();
    writeln!(stdin, r#"{{"type":"close"}}"#).unwrap();
    let events = drain_json(&mut child);
    assert!(wait_within(&mut child, Duration::from_secs(20)).success());
    assert!(events.iter().any(|event| event["event"] == "ready"));
    assert!(events.iter().any(|event| event["event"] == "info"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn kept_fork_is_one_shot_pair_safe_and_source_immutable() {
    let root = temp_root("kept-fork").canonicalize().unwrap();
    let source = root.join("source.jsonl");
    let header = serde_json::json!({"version":4,"model":"test-model","system":"test","session_id":"source-id","sandbox":root,"seat":{"id":"parent"}});
    let text = format!(
        "{header}\n{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"before\"}}]}}\n{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"call\",\"name\":\"bash\",\"input\":{{\"command\":\"touch never\"}}}}]}}\n{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"call\",\"content\":\"done\"}}]}}\n"
    );
    std::fs::write(&source, &text).unwrap();
    let invoke = |seat: &str| {
        Command::new(env!("CARGO_BIN_EXE_dext"))
            .args([
                "--fork-to",
                seat,
                "--at",
                "2",
                "--resume",
                "source.jsonl",
                "--output",
                "stream-json",
                "--cd",
            ])
            .arg(&root)
            .current_dir(&root)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &root)
            .env("DEXT_HOME", root.join(".dext"))
            .output()
            .unwrap()
    };
    let output = invoke("child");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["event"], "session_fork");
    assert_eq!(event["data"]["at"], 1);
    assert_eq!(event["data"]["source_session_id"], "source-id");
    assert_eq!(std::fs::read_to_string(&source).unwrap(), text);
    assert!(!root.join("never").exists());
    assert!(!invoke("child").status.success());
    let _ = std::fs::remove_dir_all(root);
}

struct BackgroundTestChild(Child);

impl Drop for BackgroundTestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn background_compaction_finishes_while_ndjson_is_idle_without_busy_turn() {
    use std::io::Read as _;
    let root = temp_root("background-idle").canonicalize().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (summary_started_tx, summary_started_rx) = std::sync::mpsc::channel();
    let provider = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(15);
        let mut completed = 0;
        let mut release_rx = Some(release_rx);
        let mut summary = None;
        while completed < 2 && Instant::now() < until {
            let (mut stream, _) = match listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("mock accept: {error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            let body = loop {
                let count = stream.read(&mut chunk).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&chunk[..count]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break request[end + 4..end + 4 + length].to_vec();
                    }
                }
            };
            if body.is_empty() {
                stream
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                continue;
            }
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            completed += 1;
            if request["stream"] == false {
                summary_started_tx.send(()).unwrap();
                let release = release_rx.take().unwrap();
                summary = Some(std::thread::spawn(move || {
                    release.recv_timeout(Duration::from_secs(30)).unwrap();
                    let body = r#"{"choices":[{"message":{"content":"idle background summary"}}],"usage":{"prompt_tokens":11,"completion_tokens":7}}"#;
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                }));
            } else {
                let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Foreground NDJSON answer completed while the background summary remains behind its barrier, and input remains usable.\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        }
        assert_eq!(completed, 2);
        summary.unwrap().join().unwrap();
    });
    let mut fixture = format!(
        "{}\n",
        serde_json::json!({"version":4,"model":"mock-model","system":"test","session_id":"background-source","sandbox":root,"compact_threshold_chars":30000})
    );
    for index in 0..12 {
        fixture.push_str(&format!("{}\n", serde_json::json!({"role":if index % 2 == 0 {"user"} else {"assistant"},"content":[{"type":"text","text":"context ".repeat(250)}]})));
    }
    std::fs::write(root.join("source.jsonl"), fixture).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dext"));
    command.env_clear();
    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    let mut child = BackgroundTestChild(
        command
            .args([
                "--input",
                "ndjson",
                "--output",
                "stream-json",
                "--no-session",
                "--resume=source.jsonl",
                "--cd",
            ])
            .arg(&root)
            .current_dir(&root)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &root)
            .env("DEXT_HOME", root.join(".dext"))
            .env("DEXT_PROVIDER", "local")
            .env("DEXT_BASE_URL", base)
            .env("DEXT_MODEL", "mock-model")
            .env("DEXT_MODEL_FORCE", "1")
            .env("DEXT_BACKGROUND_COMPACT", "1")
            .env("DEXT_APPROVAL", "never")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    let output = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let value: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
            let _ = tx.send(value);
        }
    });
    let mut events = Vec::new();
    loop {
        let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let ready = event["event"] == "ready";
        events.push(event);
        if ready {
            break;
        }
    }
    writeln!(input, "{{\"type\":\"user\",\"text\":\"Hello\"}}").unwrap();
    loop {
        let event = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let done = event["event"] == "turn_end";
        events.push(event);
        if done {
            break;
        }
    }
    assert!(
        !events
            .iter()
            .any(|event| event["event"] == "turn_end" && event["data"]["failed"] == true),
        "foreground request failed before the summary barrier: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "background_compaction"
                && event["data"]["phase"] == "running"),
        "summary was not started: {events:?}"
    );
    assert!(!events.iter().any(|event| event["event"] == "compact_end"));
    writeln!(input, "{{\"type\":\"control\",\"command\":\"/history\"}}").unwrap();
    loop {
        let event = rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|error| panic!("idle command: {error}; events={events:?}"));
        let usable = (event["event"] == "slash"
            || event["event"] == "info"
            || event["event"] == "structured_slash")
            && event["data"]
                .as_str()
                .is_some_and(|text| text.contains("history:"));
        events.push(event);
        if usable {
            break;
        }
    }
    summary_started_rx
        .recv_timeout(Duration::from_secs(10))
        .unwrap_or_else(|error| {
            panic!("summary request did not reach the barrier: {error}; events={events:?}")
        });
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let event = rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| {
                panic!(
                    "idle summary did not apply: {error}; child={:?}; events={events:?}",
                    child.0.try_wait()
                )
            });
        let applied =
            event["event"] == "background_compaction" && event["data"]["phase"] == "applied";
        let rejected = event["event"] == "background_compaction"
            && matches!(
                event["data"]["phase"].as_str(),
                Some("failed" | "discarded" | "cancelled")
            );
        events.push(event);
        assert!(!rejected, "idle summary was rejected: {events:?}");
        if applied {
            break;
        }
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "turn_start")
            .count(),
        1
    );
    assert!(!events.iter().any(|event| event["event"] == "compact_start"));
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "compact_end" && event["data"]["background"] == true)
    );
    writeln!(input, "{{\"type\":\"close\"}}").unwrap();
    drop(input);
    assert!(wait_within(&mut child.0, Duration::from_secs(5)).success());
    reader.join().unwrap();
    provider.join().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn eof_terminates() {
    let root = temp_root("eof");
    let mut child = spawn(&root, &[]);
    drop(child.stdin.take());
    let events = drain_json(&mut child);
    wait_within(&mut child, Duration::from_secs(20));
    assert_eq!(events[0]["event"], "ready");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn oversized_frame_is_refused_within_bound_and_closes() {
    let root = temp_root("big");
    let mut child = spawn(&root, &[]);
    let mut stdin = child.stdin.take().unwrap();
    let big = format!(
        "{{\"type\":\"user\",\"text\":\"{}\"}}\n",
        "x".repeat(300 * 1024)
    );
    let _ = stdin.write_all(big.as_bytes()); // may EPIPE once dext closes
    drop(stdin);
    let events = drain_json(&mut child);
    wait_within(&mut child, Duration::from_secs(20));
    let ack = events
        .iter()
        .find(|e| e["event"] == "input_ack")
        .expect("ack");
    assert_eq!(ack["data"]["route"], "invalid");
    assert!(ack["data"]["detail"].as_str().unwrap().contains("exceeds"));
    let _ = std::fs::remove_dir_all(&root);
}
