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
