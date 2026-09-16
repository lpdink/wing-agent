//! Streaming tool-args benchmarks (issue #98).
//!
//! Two costs, measured separately because they have different owners:
//!
//!   append/<kb>          — the arrival path: a `tool_call_stream` fragment
//!                          lands in the cell buffer. Target: O(fragment) —
//!                          the per-event cost must NOT grow with the
//!                          accumulated payload (a parse per fragment was the
//!                          O(n²) path that pinned the 256-slot event channel
//!                          during large Write streams).
//!   frames_60fps/<kb>    — the same fragments with a rendered frame every
//!                          [`FRAME_EVERY`] events (60 fps against the ~300
//!                          ev/s of a fast model). This is the amortized
//!                          cost: the deferred parse (+ incremental highlight
//!                          refresh, heights, blit) is paid once per drawn
//!                          frame, not once per fragment.
//!
//! On the first pass the distributions are printed to stderr: per-fragment
//! append cost (first vs last quarter — the scale-invariance claim from the
//! issue, ratio < 2×) and per-frame cost (p50/p99 at full payload).
//!
//! Not modelled: a `content`-before-`path` arg stream — the highlight cache's
//! language flips from unknown to known only when the path lands, which
//! triggers its documented one-off full rebuild (measured separately:
//! ~105 ms at a 512 KB markdown payload, paid once per tool call).
//!
//! Run:  cargo bench --bench tool_args_stream
//! Filter: cargo bench --bench tool_args_stream -- append/512

use std::io::Write as _;
use std::time::Instant;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;
use wing::config::LayoutConfig;
use wing::config::ThemePalette;
use wing::config::rendering::ThinkingMode;
use wing::render::renderable::CellContext;
use wing::ui::cells::tool_call::ToolCallBlock;
use wing::ui::chat_view::{ChatCell, ChatView, ChatViewWidget};

const WIDTH: u16 = 80;
const VIEWPORT: u16 = 50;
/// Fragment size: ~400 B is the 33 ms cadence of a 12 KB/s stream
/// (≈3000 tokens/s) — the same chunking the render benchmarks use.
const CHUNK: usize = 400;
const SIZES_KB: [usize; 3] = [64, 256, 512];
/// One drawn frame per five events: 60 fps frames against the ~300 ev/s of
/// a fast model (the frame gate coalesces everything in between).
const FRAME_EVERY: usize = 5;

/// Write args wire text of ≈ `kb` KB: `{"path": …, "content": "<lines>"}`.
/// Emitted in schema order (path first, like the model itself), ASCII only
/// so fragment slicing never crosses a UTF-8 boundary.
fn write_args_payload(kb: usize) -> String {
    let target = kb * 1024;
    let mut content = String::with_capacity(target);
    while content.len() < target {
        content.push_str("The quick brown fox jumps over the lazy dog 0123456789\n");
    }
    format!(
        "{{\"path\":\"/tmp/bench.md\",\"content\":{}}}",
        serde_json::to_string(&content).expect("strings always serialize")
    )
}

/// The payload split into `CHUNK`-sized fragments, as the backend sends it.
fn fragments(payload: &str) -> Vec<&str> {
    payload
        .as_bytes()
        .chunks(CHUNK)
        .map(|c| std::str::from_utf8(c).expect("ascii corpus"))
        .collect()
}

/// One streaming Write cell, driven through the public ChatView API.
struct Harness {
    view: ChatView,
    palette: ThemePalette,
    layout: LayoutConfig,
    area: Rect,
    buf: Buffer,
}

impl Harness {
    fn new() -> Self {
        let palette = ThemePalette::default();
        let layout = LayoutConfig::default();
        let mut view = ChatView::new();
        view.push(ChatCell::ToolCall(ToolCallBlock::new_streaming(
            "Write".into(),
            "tc-bench".into(),
        )));
        let area = Rect::new(0, 0, WIDTH, VIEWPORT);
        Self {
            view,
            palette,
            layout,
            area,
            buf: Buffer::empty(area),
        }
    }

    fn index(&self) -> usize {
        self.view.tool_call_index("tc-bench").expect("just pushed")
    }

    /// Arrival path: buffer the fragment (O(1), no parse).
    fn append(&mut self, fragment: &str) {
        let idx = self.index();
        self.view.append_tool_args_fragment_by_index(idx, fragment);
    }

    /// Frame boundary: the viewport draw drives the deferred flush (the
    /// parse runs at most once per cell per frame — see `CachedCell`).
    fn frame(&mut self) {
        let ctx = CellContext {
            palette: &self.palette,
            thinking_mode: ThinkingMode::Visible,
            layout: &self.layout,
        };
        ChatViewWidget::new(&mut self.view, ctx).render(self.area, &mut self.buf);
    }
}

/// Costs of one streaming pass, in nanoseconds.
struct Pass {
    /// One entry per fragment (the arrival path).
    appends: Vec<u64>,
    /// One entry per drawn frame (the flush + heights + blit).
    frames: Vec<u64>,
}

/// Drive one payload through the cell, drawing a frame every `frame_every`
/// fragments (0 = no frames: the arrival path alone).
fn streaming_pass(chunks: &[&str], frame_every: usize) -> Pass {
    let mut h = Harness::new();
    let mut pass = Pass {
        appends: Vec::with_capacity(chunks.len()),
        frames: Vec::with_capacity(chunks.len() / frame_every.max(1) + 1),
    };
    for (i, chunk) in chunks.iter().enumerate() {
        let t = Instant::now();
        h.append(chunk);
        pass.appends.push(t.elapsed().as_nanos() as u64);
        if frame_every > 0 && (i + 1) % frame_every == 0 {
            let t = Instant::now();
            h.frame();
            pass.frames.push(t.elapsed().as_nanos() as u64);
        }
    }
    pass
}

/// First/last-quarter means and percentiles, in µs.
fn report(label: &str, kb: usize, what: &str, costs: &[u64]) {
    if costs.is_empty() {
        return;
    }
    let q = (costs.len() / 4).max(1);
    let mean_us = |s: &[u64]| s.iter().sum::<u64>() as f64 / s.len().max(1) as f64 / 1000.0;
    let first = mean_us(&costs[..q]);
    let last = mean_us(&costs[costs.len() - q..]);
    let ratio = if first > 0.0 { last / first } else { 0.0 };
    let mut sorted = costs.to_vec();
    sorted.sort_unstable();
    let p50 = sorted[sorted.len() / 2] as f64 / 1000.0;
    let p99 = sorted[(sorted.len() * 99 / 100).min(sorted.len() - 1)] as f64 / 1000.0;
    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "{label} {kb} KB · {what} × {}: first-q {first:.2} µs, last-q {last:.2} µs \
         (ratio {ratio:.2}×), p50 {p50:.2} µs, p99 {p99:.2} µs",
        costs.len(),
    );
}

fn bench_tool_args(c: &mut Criterion) {
    let mut group = c.benchmark_group("tool_args_stream");
    for kb in SIZES_KB {
        let payload = write_args_payload(kb);
        let chunks = fragments(&payload);
        group.throughput(Throughput::Bytes(payload.len() as u64));

        // Arrival path only — the per-fragment cost must be payload-independent.
        let arrival = streaming_pass(&chunks, 0);
        report("append", kb, "fragments", &arrival.appends);
        group.bench_with_input(BenchmarkId::new("append", kb), &chunks, |b, chunks| {
            b.iter_batched(
                Harness::new,
                |mut h| {
                    for chunk in chunks {
                        h.append(chunk);
                    }
                },
                BatchSize::SmallInput,
            );
        });

        // The amortized model: frames at the 60 fps gate, payload growing
        // underneath them.
        let framed = streaming_pass(&chunks, FRAME_EVERY);
        report("frames_60fps", kb, "frames", &framed.frames);
        group.bench_with_input(
            BenchmarkId::new("frames_60fps", kb),
            &chunks,
            |b, chunks| {
                b.iter_batched(
                    Harness::new,
                    |mut h| {
                        for (i, chunk) in chunks.iter().enumerate() {
                            h.append(chunk);
                            if (i + 1) % FRAME_EVERY == 0 {
                                h.frame();
                            }
                        }
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_tool_args);
criterion_main!(benches);
