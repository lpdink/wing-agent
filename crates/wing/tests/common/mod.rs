//! Shared corpus generation + baseline (full-render) frame harness for the
//! streaming-render benchmarks and tests.
//!
//! All corpora are generated deterministically from fixed seeds — no external
//! corpus files, benchmark numbers are reproducible from the repo alone.
//!
//! `BaselineCell::frame()` replicates the CURRENT production per-frame path
//! for a streaming Thinking / AssistantMessage cell:
//!
//! 1. `to_lines()` — full markdown render (parse + wrap + span rebuild)
//! 2. `compute_height` — `lines.to_vec()` + `Paragraph::line_count`
//! 3. render loop — `lines.to_vec()` + `Paragraph::render` pinned to bottom
//!
//! This is the "before" engine every optimization is measured against.

#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::Widget;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};
use wing::config::LayoutConfig;
use wing::config::ThemePalette;
use wing::config::rendering::ThinkingMode;
use wing::render::markdown::stream::{Profile, StreamingRender};
use wing::render::renderable::CellContext;
use wing::ui::cells::thinking::ThinkingBlock;
use wing::ui::chat_view::ChatCell;

// ============================================================
// Seeded RNG (splitmix64 — no external dependency)
// ============================================================

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed
            .wrapping_mul(0x9E3779B97F4A7C15)
            .wrapping_add(0x1234_5678_9ABC_DEF0))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform value below `n` (n > 0).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    /// Pick a random element.
    pub fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len() as u64) as usize]
    }
}

// ============================================================
// Corpus generation
// ============================================================

pub const SCENARIOS: &[&str] = &["thinking", "content", "code_block"];

/// Sizes in KB covered by the frame-cost curve.
pub const SIZES_KB: &[usize] = &[8, 64, 256, 512];

const EN_WORDS: &[&str] = &[
    "streaming",
    "render",
    "incremental",
    "markdown",
    "token",
    "delta",
    "frame",
    "budget",
    "cache",
    "stable",
    "prefix",
    "tail",
    "block",
    "fence",
    "paragraph",
    "list",
    "table",
    "highlight",
    "syntax",
    "reconcile",
    "viewport",
    "scroll",
    "blink",
    "latency",
    "throughput",
];

const CJK_WORDS: &[&str] = &[
    "流式",
    "渲染",
    "增量",
    "性能",
    "基准",
    "测试",
    "缓存",
    "稳定",
    "前缀",
    "尾部",
    "分块",
    "边界",
    "列表",
    "表格",
    "代码",
    "高亮",
    "折行",
    "视口",
    "滚动",
    "闪烁",
    "延迟",
    "吞吐",
    "令牌",
    "事件",
    "会话",
    "回滚",
    "对账",
    "一致性",
    "正确性",
    "复杂度",
];

const RUST_LINES: &[&str] = &[
    "let mut acc = Vec::with_capacity(n);",
    "acc.push(chunk.parse::<u64>()?);",
    "fn fold_line(buf: &mut String, seg: &Segment) {",
    "    buf.push_str(&seg.text);",
    "}",
    "match event {",
    "    Event::Text(t) => append(t),",
    "    Event::Code(c) => push_code(c),",
    "    Event::SoftBreak => flush(),",
    "    _ => {}",
    "}",
    "let width = area.width.saturating_sub(2);",
    "if !cached_valid { lines = to_lines(width, ctx); }",
    "for (i, line) in md_lines.iter().enumerate() {",
    "    let prefix = if i == 0 { \"⦁ \" } else { \"  \" };",
    "    spans.push(Span::styled(prefix, style));",
    "}",
    "let height = Paragraph::new(lines).wrap(Wrap { trim: false }).line_count(w);",
    "struct CachedHeight { width: u16, generation: u64, height: usize }",
    "impl<'a> ChatViewWidget<'a> {",
    "    pub fn new(view: &'a mut ChatView, ctx: CellContext<'a>) -> Self { Self { view, ctx } }",
    "}",
    "let skip = scroll.saturating_sub(cell_start);",
    "buf[(x, y)].set_style(bg);",
    "// measure the frame cost at this prefix",
    "let start = std::time::Instant::now();",
    "frame_times.push(start.elapsed());",
    "let p99 = sorted[sorted.len() * 99 / 100];",
    "tracing::debug!(?scenario, ?kb, \"pass complete\");",
];

fn en_sentence(rng: &mut Rng) -> String {
    let words = 6 + rng.below(18) as usize;
    let mut s = String::new();
    for i in 0..words {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(rng.pick(EN_WORDS));
    }
    s
}

fn cjk_sentence(rng: &mut Rng) -> String {
    let words = 4 + rng.below(12) as usize;
    let mut s = String::new();
    for i in 0..words {
        if i > 0 && rng.below(4) == 0 {
            s.push('，');
        }
        s.push_str(rng.pick(CJK_WORDS));
    }
    s.push('。');
    s
}

/// A mixed CJK/English sentence with occasional inline code — reasoning-style.
fn mixed_sentence(rng: &mut Rng) -> String {
    let mut s = if rng.below(2) == 0 {
        cjk_sentence(rng)
    } else {
        let mut e = en_sentence(rng);
        e.push('.');
        e
    };
    if rng.below(6) == 0 {
        s.push(' ');
        s.push('`');
        s.push_str(rng.pick(EN_WORDS));
        s.push('`');
    }
    s
}

/// Long Reasoning text: paragraphs of mixed CJK/EN prose with inline code,
/// occasional short code blocks / lists / headings, blank-line separated.
fn thinking_corpus(target: usize) -> String {
    let mut rng = Rng::new(0x7A11);
    let mut out = String::new();
    while out.len() < target {
        let roll = rng.below(100);
        if roll < 70 {
            // paragraph: 2-6 sentences on one source line, occasionally two lines
            let sentences = 2 + rng.below(5) as usize;
            let mut para = String::new();
            for i in 0..sentences {
                if i > 0 {
                    para.push(' ');
                }
                para.push_str(&mixed_sentence(&mut rng));
            }
            out.push_str(&para);
            out.push('\n');
        } else if roll < 78 {
            // short code block
            out.push_str("```rust\n");
            let n = 2 + rng.below(5) as usize;
            for _ in 0..n {
                out.push_str(rng.pick(RUST_LINES));
                out.push('\n');
            }
            out.push_str("```\n");
        } else if roll < 86 {
            // list
            let n = 3 + rng.below(4) as usize;
            for _ in 0..n {
                out.push_str("- ");
                out.push_str(&mixed_sentence(&mut rng));
                out.push('\n');
            }
        } else if roll < 90 {
            out.push_str("## ");
            out.push_str(&en_sentence(&mut rng));
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

fn table_block(rng: &mut Rng) -> String {
    let cols = 3 + rng.below(3) as usize;
    let rows = 3 + rng.below(5) as usize;
    let mut out = String::from("| ");
    for c in 0..cols {
        if c > 0 {
            out.push_str(" | ");
        }
        out.push('列');
        out.push_str(&c.to_string());
    }
    out.push_str(" |\n|");
    for _ in 0..cols {
        out.push_str("----|");
    }
    out.push('\n');
    for _ in 0..rows {
        out.push_str("| ");
        for c in 0..cols {
            if c > 0 {
                out.push_str(" | ");
            }
            out.push_str(&en_sentence(rng));
        }
        out.push_str(" |\n");
    }
    out
}

/// Mixed markdown document: headings, rich paragraphs, lists, tables,
/// nested blockquotes and long Rust code blocks.
fn content_corpus(target: usize) -> String {
    let mut rng = Rng::new(0xC0DE);
    let mut out = String::new();
    while out.len() < target {
        let roll = rng.below(100);
        if roll < 35 {
            let sentences = 2 + rng.below(4) as usize;
            let mut para = String::new();
            for i in 0..sentences {
                if i > 0 {
                    para.push(' ');
                }
                if rng.below(5) == 0 {
                    para.push_str("**");
                    para.push_str(&mixed_sentence(&mut rng));
                    para.push_str("**");
                } else if rng.below(8) == 0 {
                    para.push('[');
                    para.push_str(rng.pick(EN_WORDS));
                    para.push_str("](https://example.com/");
                    para.push_str(rng.pick(EN_WORDS));
                    para.push(')');
                } else {
                    para.push_str(&mixed_sentence(&mut rng));
                }
            }
            out.push_str(&para);
            out.push('\n');
        } else if roll < 47 {
            out.push_str("```rust\n");
            let n = 10 + rng.below(50) as usize;
            for _ in 0..n {
                out.push_str(rng.pick(RUST_LINES));
                out.push('\n');
            }
            out.push_str("```\n");
        } else if roll < 59 {
            let n = 3 + rng.below(6) as usize;
            for _ in 0..n {
                out.push_str("- ");
                out.push_str(&mixed_sentence(&mut rng));
                out.push('\n');
            }
        } else if roll < 67 {
            out.push_str(&table_block(&mut rng));
        } else if roll < 75 {
            out.push_str("> ");
            out.push_str(&mixed_sentence(&mut rng));
            out.push('\n');
            if rng.below(2) == 0 {
                out.push_str(">> ");
                out.push_str(&mixed_sentence(&mut rng));
                out.push('\n');
            }
            out.push_str("> ");
            out.push_str(&mixed_sentence(&mut rng));
            out.push('\n');
        } else if roll < 83 {
            out.push_str("### ");
            out.push_str(&en_sentence(&mut rng));
            out.push('\n');
        } else if roll < 88 {
            out.push_str("1. ");
            out.push_str(&mixed_sentence(&mut rng));
            out.push('\n');
            out.push_str("2. ");
            out.push_str(&mixed_sentence(&mut rng));
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

/// Code-dominated stream: giant Rust code blocks with minimal prose — the
/// worst case for unclosed-code-block streaming.
fn code_corpus(target: usize) -> String {
    let mut rng = Rng::new(0xB10C);
    let mut out = String::new();
    out.push_str("Refactor the render loop:\n\n");
    while out.len() < target {
        out.push_str("```rust\n");
        let n = 60 + rng.below(240) as usize;
        for _ in 0..n {
            out.push_str(rng.pick(RUST_LINES));
            out.push('\n');
        }
        out.push_str("```\n\n");
        if rng.below(3) == 0 {
            out.push_str(&mixed_sentence(&mut rng));
            out.push_str("\n\n");
        }
    }
    out
}

/// Deterministic corpus for a scenario, approximately `target_bytes` bytes.
pub fn corpus(scenario: &str, target_bytes: usize) -> String {
    match scenario {
        "thinking" => thinking_corpus(target_bytes),
        "content" => content_corpus(target_bytes),
        "code_block" => code_corpus(target_bytes),
        _ => panic!("unknown scenario: {scenario}"),
    }
}

/// Split text into chunks of ~`chunk_bytes` bytes, always cutting at char
/// boundaries (corpora contain CJK — never split mid-char).
pub fn chunk_stream(text: &str, chunk_bytes: usize) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + chunk_bytes).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

/// Seeded random chunk sizes — exercises arbitrary split points.
pub fn random_chunks(text: &str, seed: u64, max_chunk: usize) -> Vec<&str> {
    let mut rng = Rng::new(seed);
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + 1 + rng.below(max_chunk as u64) as usize).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

// ============================================================
// Baseline (current production) frame harness
// ============================================================

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BaselineKind {
    /// ThinkingBlock — reasoning stream.
    Thinking,
    /// ChatCell::AssistantMessage — content stream.
    Assistant,
}

/// A streaming cell driven exactly like the production app drives it today:
/// every delta appends, and `frame()` performs one full-render draw cycle.
pub struct BaselineCell {
    kind: BaselineKind,
    text: String,
    thinking: ThinkingBlock,
    palette: ThemePalette,
    layout: LayoutConfig,
}

impl BaselineCell {
    pub fn new(kind: BaselineKind) -> Self {
        Self {
            kind,
            text: String::new(),
            thinking: ThinkingBlock::new(),
            palette: ThemePalette::default(),
            layout: LayoutConfig::default(),
        }
    }

    /// Append a streaming delta.
    pub fn push(&mut self, delta: &str) {
        self.text.push_str(delta);
        self.thinking.append(delta);
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// One production-equivalent frame of the CURRENT full-render path:
    /// markdown render → clone + line_count (height) → clone + Paragraph
    /// render pinned to the viewport bottom. Returns the cell height.
    pub fn frame(&mut self, width: u16, viewport: u16) -> usize {
        let lines = self.render_lines(width);
        // compute_height path: to_vec + line_count.
        let height = Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .line_count(width);
        // Render-loop path: to_vec + Paragraph::render, auto-scroll pinned
        // to the bottom of the content.
        let area = Rect::new(0, 0, width, viewport.max(1));
        let mut buf = Buffer::empty(area);
        let skip = height.saturating_sub(viewport.max(1) as usize) as u16;
        Paragraph::new(lines.to_vec())
            .wrap(Wrap { trim: false })
            .scroll((skip, 0))
            .render(area, &mut buf);
        height
    }

    /// Full-render lines for the accumulated text (no Paragraph overhead) —
    /// the same output the production cell would produce.
    pub fn render_lines(&self, width: u16) -> Vec<Line<'static>> {
        match self.kind {
            BaselineKind::Thinking => {
                self.thinking
                    .to_lines(&self.palette, ThinkingMode::Visible, width)
            }
            BaselineKind::Assistant => {
                let ctx = CellContext {
                    palette: &self.palette,
                    thinking_mode: ThinkingMode::Visible,
                    layout: &self.layout,
                };
                ChatCell::AssistantMessage(self.text.clone()).to_lines(width, &ctx)
            }
        }
    }
}

// ============================================================
// Streaming engine harness (baseline vs incremental)
// ============================================================

/// Which rendering engine a [`StreamCell`] drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    /// The current production path — full markdown re-render per frame.
    Baseline,
    /// The incremental `StreamingRender` engine (stable prefix + tail).
    Incremental,
}

/// A streaming cell driven by either engine — the comparison harness for
/// benchmarks and the throughput judgment.
pub struct StreamCell {
    engine: Engine,
    baseline: BaselineCell,
    stream: Option<StreamingRender>,
}

impl StreamCell {
    pub fn new(kind: BaselineKind, engine: Engine) -> Self {
        let profile = match kind {
            BaselineKind::Thinking => Profile::Thinking,
            BaselineKind::Assistant => Profile::Content,
        };
        Self {
            engine,
            baseline: BaselineCell::new(kind),
            stream: match engine {
                Engine::Baseline => None,
                Engine::Incremental => Some(StreamingRender::new(profile)),
            },
        }
    }

    /// Append a streaming delta (both engines stay in sync).
    pub fn push(&mut self, delta: &str) {
        self.baseline.push(delta);
        if let Some(stream) = self.stream.as_mut() {
            stream.push(delta);
        }
    }

    /// One frame: render the current accumulated text at `width` and
    /// return the cell height.
    ///
    /// - Baseline: full markdown render + clone + line_count + Paragraph
    ///   render (the production per-frame path).
    /// - Incremental: `StreamingRender::lines()` — promote closed blocks,
    ///   re-render only the active tail; height is the flat line count.
    pub fn frame(&mut self, width: u16, viewport: u16) -> usize {
        match self.engine {
            Engine::Baseline => self.baseline.frame(width, viewport),
            Engine::Incremental => {
                let stream = self
                    .stream
                    .as_mut()
                    .expect("incremental engine must have a stream");
                let lines = stream.lines(width, &ThemePalette::default());
                lines.len()
            }
        }
    }

    /// Incremental engine's final reconcile render (turn end).
    pub fn finalize(&mut self, width: u16) {
        if let Some(stream) = self.stream.as_mut() {
            stream.finalize(width, &ThemePalette::default());
        }
    }
}

// ============================================================
// Per-frame distribution stats
// ============================================================

/// Average / p50 / p99 / max over per-frame microsecond samples.
pub struct FrameStats {
    pub avg_us: u64,
    pub p50_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub frames: usize,
}

pub fn frame_stats(samples_us: &[u64]) -> FrameStats {
    let mut sorted = samples_us.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let idx = |q: usize| sorted[(n * q / 100).min(n - 1)];
    FrameStats {
        avg_us: samples_us.iter().sum::<u64>() / n.max(1) as u64,
        p50_us: idx(50),
        p99_us: idx(99),
        max_us: sorted[n - 1],
        frames: n,
    }
}
