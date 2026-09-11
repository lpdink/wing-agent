//! Throughput judgment for streaming rendering.
//!
//! Pumps a corpus as ~400B delta events (the 33ms cadence of a 12 KB/s
//! stream ≈ 3000 tokens/s) and measures the per-frame render cost
//! distribution. The stream is sustainable iff:
//!   - avg frame cost ≤ 33ms  (consumption keeps up with arrival — no backlog)
//!   - p99 frame cost < 16ms  (frame budget headroom)
//!
//! Before optimization the baseline engine FAILS by design — run it with
//!   WING_ENGINE=baseline cargo test --release --test stream_render_throughput -- --ignored --nocapture
//! and the measured numbers are recorded in tasks.md §6. The default
//! engine is the incremental one; the assertions document the target.

mod common;

use std::io::Write as _;
use std::time::Instant;

use common::{BaselineKind, Engine, SCENARIOS, StreamCell, chunk_stream, corpus, frame_stats};

const WIDTH: u16 = 80;
const VIEWPORT: u16 = 50;
const CHUNK: usize = 400;

/// Budget per event at 12 KB/s with 400B chunks (33ms cadence).
const ARRIVAL_BUDGET_US: u64 = 33_000;
/// Render frame budget.
const FRAME_BUDGET_US: u64 = 16_000;

fn scenario_kind(scenario: &str) -> BaselineKind {
    match scenario {
        "thinking" => BaselineKind::Thinking,
        _ => BaselineKind::Assistant,
    }
}

#[test]
#[ignore = "long-running throughput judgment; run with --release -- --ignored --nocapture"]
fn throughput_judgment() {
    // Engine under judgment: incremental (the target); baseline is
    // re-runnable for comparison via WING_ENGINE=baseline.
    let engine = match std::env::var("WING_ENGINE").as_deref() {
        Ok("baseline") => Engine::Baseline,
        _ => Engine::Incremental,
    };
    // Optional single-scenario filter (e.g. WING_SCENARIO=thinking) for
    // quick re-measurement of one profile.
    let selected: Vec<&'static str> = match std::env::var("WING_SCENARIO") {
        Ok(s) if SCENARIOS.contains(&s.as_str()) => {
            vec![SCENARIOS.iter().copied().find(|sc| *sc == s).unwrap()]
        }
        _ => SCENARIOS.to_vec(),
    };
    let mut failures: Vec<String> = Vec::new();

    for &scenario in &selected {
        for &kb in &[64usize, 256, 512] {
            let corpus = corpus(scenario, kb * 1024);
            let chunks = chunk_stream(&corpus, CHUNK);
            let kind = scenario_kind(scenario);

            let mut cell = StreamCell::new(kind, engine);
            let mut frames_us = Vec::with_capacity(chunks.len());
            for chunk in &chunks {
                cell.push(chunk);
                let t = Instant::now();
                cell.frame(WIDTH, VIEWPORT);
                frames_us.push(t.elapsed().as_micros() as u64);
            }
            // Turn-end reconcile render (incremental engine only) —
            // included in the pass, not in the per-frame distribution.
            if engine == Engine::Incremental {
                cell.finalize(WIDTH);
            }

            let s = frame_stats(&frames_us);
            let _ = writeln!(
                std::io::stderr(),
                "engine={engine:?} {scenario:<11} {kb:>4}KB  frames={frames:<6} \
                 avg={avg:>7}µs  p50={p50:>7}µs  p99={p99:>7}µs  max={max:>7}µs",
                frames = s.frames,
                avg = s.avg_us,
                p50 = s.p50_us,
                p99 = s.p99_us,
                max = s.max_us,
            );

            if s.avg_us > ARRIVAL_BUDGET_US {
                failures.push(format!(
                    "{scenario}@{kb}KB: avg frame {}µs > {}µs arrival budget (backlog)",
                    s.avg_us, ARRIVAL_BUDGET_US
                ));
            }
            if s.p99_us >= FRAME_BUDGET_US {
                failures.push(format!(
                    "{scenario}@{kb}KB: p99 frame {}µs >= {}µs frame budget",
                    s.p99_us, FRAME_BUDGET_US
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "streaming render cannot sustain 3000 tokens/s (12KB/s), engine={engine:?}:\n  {}",
        failures.join("\n  ")
    );
}
