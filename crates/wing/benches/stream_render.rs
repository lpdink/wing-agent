//! Streaming-render benchmarks.
//!
//! Measures the per-frame cost of driving a streaming Thinking /
//! AssistantMessage cell through either engine:
//!
//!   baseline/<scenario>/<kb>    — the current production path: one full
//!                                 markdown re-render per delta frame.
//!   incremental/<scenario>/<kb> — the StreamingRender engine: promote
//!                                 closed blocks once, re-render only the
//!                                 active tail.
//!
//! Criterion reports the full streaming pass; the per-frame distribution
//! (avg/p50/p99/max) is printed to stderr on the first pass.
//!
//! Run:  cargo bench --bench stream_render
//! Filter examples:
//!   cargo bench --bench stream_render -- 'incremental/thinking'
//!   cargo bench --bench stream_render -- 'baseline/.*/64'
//!
//! Frame-budget judgment (p99 < 16ms @ 512KB) lives in
//! tests/stream_render_throughput.rs; span-exactness reconciliation lives in
//! tests/stream_render_reconcile.rs.

#[path = "../tests/common/mod.rs"]
mod common;

use std::io::Write as _;
use std::time::{Duration, Instant};

use common::{
    BaselineKind, Engine, SCENARIOS, SIZES_KB, StreamCell, chunk_stream, corpus, frame_stats,
};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

/// Terminal width / chat viewport height used for every frame.
const WIDTH: u16 = 80;
const VIEWPORT: u16 = 50;

/// Delta chunk size: ~400 bytes per event ≈ the 33ms cadence of a
/// 12 KB/s stream (≈3000 tokens/s).
const CHUNK: usize = 400;

fn scenario_kind(scenario: &str) -> BaselineKind {
    match scenario {
        "thinking" => BaselineKind::Thinking,
        _ => BaselineKind::Assistant,
    }
}

/// One full streaming pass over a corpus with the given engine; prints the
/// per-frame distribution on the first pass.
fn streaming_pass(
    scenario: &str,
    kb: usize,
    engine: Engine,
    chunks: &[&str],
    report: bool,
) -> Duration {
    let kind = scenario_kind(scenario);
    let start = Instant::now();
    let mut cell = StreamCell::new(kind, engine);
    let mut frames_us: Vec<u64> = Vec::with_capacity(chunks.len());
    let mut pushes_us: Vec<u64> = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let t = Instant::now();
        cell.push(chunk);
        let dt = t.elapsed().as_micros() as u64;
        if report {
            pushes_us.push(dt);
        }
        let t = Instant::now();
        cell.frame(WIDTH, VIEWPORT);
        let dt = t.elapsed().as_micros() as u64;
        if report {
            frames_us.push(dt);
        }
    }
    if engine == Engine::Incremental {
        cell.finalize(WIDTH);
    }
    if report {
        let f = frame_stats(&frames_us);
        let p = frame_stats(&pushes_us);
        let _ = writeln!(
            std::io::stderr(),
            "engine={engine:?} scenario={scenario} size={kb}KB frames={} \
             frame avg={}µs p50={}µs p99={}µs max={}µs | push avg={}µs p99={}µs",
            f.frames,
            f.avg_us,
            f.p50_us,
            f.p99_us,
            f.max_us,
            p.avg_us,
            p.p99_us,
        );
    }
    start.elapsed()
}

fn bench_engine(c: &mut Criterion, engine: Engine) {
    let prefix = match engine {
        Engine::Baseline => "baseline",
        Engine::Incremental => "incremental",
    };
    for &scenario in SCENARIOS {
        // The baseline re-renders the whole text per frame regardless of
        // fence structure, so `giant_fence` only measures the incremental
        // engine's O(new lines) behavior — `code_block` already covers the
        // baseline's cost (and would take minutes on a 512 KB single fence).
        if engine == Engine::Baseline && scenario == "giant_fence" {
            continue;
        }
        let mut group = c.benchmark_group(format!("{prefix}/{scenario}"));
        group.sample_size(10);
        group.warm_up_time(Duration::from_secs(1));
        for &kb in SIZES_KB {
            let corpus = corpus(scenario, kb * 1024);
            let chunks = chunk_stream(&corpus, CHUNK);
            group.throughput(Throughput::Bytes(corpus.len() as u64));
            group.bench_with_input(BenchmarkId::from_parameter(kb), &kb, |b, &kb| {
                let mut reported = false;
                b.iter_custom(|iters| {
                    let mut total = Duration::ZERO;
                    for _ in 0..iters {
                        total += streaming_pass(scenario, kb, engine, &chunks, !reported);
                        reported = true;
                    }
                    total
                });
            });
        }
        group.finish();
    }
}

fn bench_baseline(c: &mut Criterion) {
    bench_engine(c, Engine::Baseline);
}

fn bench_incremental(c: &mut Criterion) {
    bench_engine(c, Engine::Incremental);
}

criterion_group!(benches, bench_baseline, bench_incremental);
criterion_main!(benches);
