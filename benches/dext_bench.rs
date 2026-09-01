use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use dext::sse::SseDecoder;
use serde_json::json;

#[allow(dead_code)]
#[path = "../src/read_symbol.rs"]
mod read_symbol;

fn build_sse_buffer(event_count: usize) -> Vec<u8> {
    let mut buffer = String::with_capacity(event_count * 220);
    for index in 0..event_count {
        let payload = json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "text_delta",
                "text": format!("chunk number {index} of streamed text ")
            }
        });
        buffer.push_str("event: content_block_delta\n");
        buffer.push_str("data: ");
        buffer.push_str(&payload.to_string());
        buffer.push_str("\n\n");
    }
    buffer.into_bytes()
}

fn decode_sse(buffer: &[u8], chunk_size: usize) -> usize {
    let mut decoder = SseDecoder::new(64 * 1024);
    let mut frames = 0usize;
    for chunk in buffer.chunks(chunk_size) {
        frames += decoder.push(chunk).expect("decode benchmark SSE").len();
    }
    frames + decoder.finish().expect("finish benchmark SSE").len()
}

fn bench_sse_decode(c: &mut Criterion) {
    const EVENT_COUNT: usize = 512;
    let buffer = build_sse_buffer(EVENT_COUNT);
    let bytes = buffer.len() as u64;
    let mut group = c.benchmark_group("production_sse_decode");
    group.throughput(Throughput::Bytes(bytes));
    group.measurement_time(Duration::from_secs(6));
    group.bench_function("coalesced_provider_read", |b| {
        b.iter(|| {
            let frames = decode_sse(black_box(&buffer), buffer.len());
            assert_eq!(frames, EVENT_COUNT);
            black_box(frames);
        })
    });
    group.bench_function("fragmented_97_byte_reads", |b| {
        b.iter(|| {
            let frames = decode_sse(black_box(&buffer), 97);
            assert_eq!(frames, EVENT_COUNT);
            black_box(frames);
        })
    });
    group.finish();
}

fn bench_read_symbol(c: &mut Criterion) {
    let mut source = String::with_capacity(512 * 1024);
    for index in 0..4_000 {
        source.push_str(&format!(
            "impl Type{index} {{ fn build() {{ let text = r#\"{{}}\"#; }} }}\n"
        ));
    }
    let bytes = source.len() as u64;
    let mut group = c.benchmark_group("read_symbol_rust_ast");
    group.throughput(Throughput::Bytes(bytes));
    group.measurement_time(Duration::from_secs(6));
    group.bench_function("qualified_last_item", |b| {
        b.iter(|| {
            let output = read_symbol::read(
                black_box(&source),
                "fixture.rs",
                true,
                read_symbol::Selector::Symbol("Type3999::build"),
                0,
                16_000,
            )
            .expect("benchmark symbol lookup");
            black_box(output)
        })
    });
    group.finish();
}

criterion_group!(benches, bench_sse_decode, bench_read_symbol);
criterion_main!(benches);
