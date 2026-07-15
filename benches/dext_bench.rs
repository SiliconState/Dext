use std::fs;
use std::hint::black_box;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use serde_json::{Value, json};

const TEXT_TOOL_CAPTURE_CAP: usize = 10_000;
const LATEST_LOG_CAP: usize = 64_000;

fn unique_tmp(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    p.push(format!("dext-bench-{tag}-{nanos}-{}", std::process::id()));
    p
}

fn build_sse_buffer(n_events: usize) -> String {
    let mut s = String::with_capacity(n_events * 220);
    for i in 0..n_events {
        let payload = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": format!("chunk number {i} of streamed text ") }
        });
        s.push_str("event: content_block_delta\n");
        s.push_str("data: ");
        s.push_str(&payload.to_string());
        s.push_str("\n\n");
    }
    s
}

fn parse_sse_buffer(buf: &str) -> usize {
    let mut parsed = 0usize;
    for block in buf.split("\n\n") {
        if block.is_empty() {
            continue;
        }
        for line in block.split('\n') {
            if let Some(rest) = line.strip_prefix("data: ") {
                if rest == "[DONE]" {
                    continue;
                }
                if serde_json::from_str::<Value>(rest).is_ok() {
                    parsed += 1;
                }
            }
        }
    }
    parsed
}

fn bench_sse_parse(c: &mut Criterion) {
    let buf = build_sse_buffer(512);
    let bytes = buf.len() as u64;
    let mut g = c.benchmark_group("sse_parse");
    g.throughput(Throughput::Bytes(bytes));
    g.measurement_time(Duration::from_secs(6));
    g.bench_function("stream_512_deltas", |b| {
        b.iter(|| {
            let n = parse_sse_buffer(black_box(&buf));
            black_box(n);
        })
    });
    g.finish();
}

fn prepare_text_file(path: &std::path::Path, kb: usize) {
    let mut content = String::with_capacity(kb * 1024 + 256);
    let mut i = 0usize;
    while content.len() < kb * 1024 {
        content.push_str(&format!(
            "line {i:06} payload: the quick brown fox jumps over the lazy dog; pack my box with five dozen liquor jugs\n"
        ));
        i += 1;
    }
    fs::write(path, content).expect("write test file");
}

fn read_file_equivalent(path: &std::path::Path) -> String {
    let file = fs::File::open(path).expect("open");
    let reader = BufReader::new(file);
    let mut out = String::with_capacity(TEXT_TOOL_CAPTURE_CAP + 1024);
    for (i, line) in reader.lines().enumerate() {
        let line = line.expect("utf8 line");
        let line_no = i + 1;
        let rendered = format!("{line_no}\t{line}\n");
        if out.len() + rendered.len() > TEXT_TOOL_CAPTURE_CAP {
            break;
        }
        out.push_str(&rendered);
    }
    out
}

fn bench_read_file(c: &mut Criterion) {
    let path = unique_tmp("readfile");
    prepare_text_file(&path, 100);
    let size = fs::metadata(&path).unwrap().len();
    let mut g = c.benchmark_group("tool_read_file");
    g.throughput(Throughput::Bytes(size));
    g.measurement_time(Duration::from_secs(5));
    g.bench_function("100k_linebuf", |b| {
        b.iter(|| {
            let s = read_file_equivalent(black_box(&path));
            black_box(s.len());
        })
    });
    g.finish();
    let _ = fs::remove_file(&path);
}

fn build_session_messages(n: usize) -> Vec<Value> {
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        v.push(json!({
            "role": role,
            "content": [
                { "type": "text", "text": format!("message {i} body with a moderate amount of text so the serialized size is realistic for a chat turn.") }
            ]
        }));
    }
    v
}

fn bench_session_serialize(c: &mut Criterion) {
    let msgs = build_session_messages(1000);
    let mut g = c.benchmark_group("session_serialize");
    g.measurement_time(Duration::from_secs(5));
    g.bench_function("1k_messages_to_string", |b| {
        b.iter(|| {
            let s = serde_json::to_string(black_box(&msgs)).unwrap();
            black_box(s.len());
        })
    });
    g.bench_function("1k_messages_to_jsonl", |b| {
        b.iter(|| {
            let mut out = String::with_capacity(256 * msgs.len());
            for m in black_box(&msgs) {
                out.push_str(&serde_json::to_string(m).unwrap());
                out.push('\n');
            }
            black_box(out.len());
        })
    });
    g.finish();
}

fn atomic_write_equivalent(path: &std::path::Path, data: &[u8]) {
    let mut tmp = path.to_path_buf();
    let fname = tmp.file_name().map(|n| n.to_owned()).unwrap_or_default();
    tmp.set_file_name(format!(
        "{}.tmp.{}",
        fname.to_string_lossy(),
        std::process::id()
    ));
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .expect("open tmp");
        f.write_all(data).expect("write tmp");
        f.sync_data().ok();
    }
    fs::rename(&tmp, path).expect("rename");
}

fn append_log_rewrite(path: &std::path::Path, line: &str) {
    let existing = fs::read(path).unwrap_or_default();
    let mut data = String::from_utf8_lossy(&existing).into_owned();
    if !data.is_empty() && !data.ends_with('\n') {
        data.push('\n');
    }
    data.push_str(line);
    data.push('\n');
    if data.len() > LATEST_LOG_CAP {
        let start = data.len() - LATEST_LOG_CAP;
        let mut idx = start;
        while idx < data.len() && !data.is_char_boundary(idx) {
            idx += 1;
        }
        data = data[idx..].to_string();
    }
    atomic_write_equivalent(path, data.as_bytes());
}

fn append_log_fast(path: &std::path::Path, line: &str) {
    let current_len = fs::metadata(path).map(|m| m.len()).unwrap_or(0) as usize;
    let needed = line.len() + 1;
    if current_len + needed <= LATEST_LOG_CAP
        && let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path)
    {
        let mut buf = String::with_capacity(needed);
        buf.push_str(line);
        buf.push('\n');
        let _ = f.write_all(buf.as_bytes());
        return;
    }
    append_log_rewrite(path, line);
}

fn bench_log_append(c: &mut Criterion) {
    let mut g = c.benchmark_group("log_append");
    g.measurement_time(Duration::from_secs(5));

    let path_old = unique_tmp("logappend-rewrite");
    g.bench_function("rewrite_100_events_capped_64k", |b| {
        b.iter(|| {
            let _ = fs::remove_file(&path_old);
            for i in 0..100 {
                append_log_rewrite(
                    &path_old,
                    &format!("[{i}] evt stream_chunk detail=bench-run payload=filler"),
                );
            }
        })
    });
    let _ = fs::remove_file(&path_old);

    let path_fast = unique_tmp("logappend-fast");
    g.bench_function("fast_100_events_capped_64k", |b| {
        b.iter(|| {
            let _ = fs::remove_file(&path_fast);
            for i in 0..100 {
                append_log_fast(
                    &path_fast,
                    &format!("[{i}] evt stream_chunk detail=bench-run payload=filler"),
                );
            }
        })
    });
    let _ = fs::remove_file(&path_fast);

    g.finish();
}

fn bench_json_parse_small(c: &mut Criterion) {
    let sample = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello world this is a sample delta chunk"}}"#;
    let mut g = c.benchmark_group("json_parse");
    g.throughput(Throughput::Bytes(sample.len() as u64));
    g.bench_function("sse_delta_event", |b| {
        b.iter(|| {
            let v: Value = serde_json::from_str(black_box(sample)).unwrap();
            black_box(v);
        })
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_sse_parse,
    bench_read_file,
    bench_session_serialize,
    bench_log_append,
    bench_json_parse_small,
);
criterion_main!(benches);
