//! Span-exactness reconciliation matrix for streaming rendering.
//!
//! For every (corpus shape × chunk size × width) combination, the
//! incremental `StreamingRender` output must equal the reference full
//! render ([`full_lines`]) span by span (text + style) — for BOTH profiles
//! (Thinking renders code plain; Content keeps highlighting).

mod common;

use std::fmt::Write as _;

use common::{chunk_stream, random_chunks};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use wing::config::ThemePalette;
use wing::render::markdown::stream::{Profile, StreamingRender, full_lines};

// ============================================================
// Corpus shapes — handcrafted markdown forms the splitter must survive
// ============================================================

fn shapes() -> Vec<(&'static str, String)> {
    vec![
        ("plain_paragraphs", "First paragraph of plain text.\n\nSecond paragraph here.\n\nThird.".into()),
        ("cjk_prose", "这是一段中文推理文本，包含较长的中文段落，用于验证 CJK 折行。\n\n第二段：流式渲染需要处理中文标点的行尾禁则（kinsoku）。\n\n混合 English 与中文 sentence 的段落。".into()),
        ("unclosed_fence", "Intro text before the block.\n\n```rust\nlet x = 1;\nlet y = 2;\n".into()),
        ("fence_with_blank_lines", "```\ncode line one\n\n\ncode line two after blanks\n```\n\nAfter the block.".into()),
        ("fence_glued_to_text", "text:```python\nlet x = 1;\n```\n\nAfter.".into()),
        ("table", "| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\nAfter table.".into()),
        ("loose_list", "- first item\n\n- second item\n\n- third item\n\nAfter list.".into()),
        ("tight_list", "- a\n- b\n- c\n\nAfter.".into()),
        ("setext_heading", "Title line\n===\n\nBody paragraph.".into()),
        ("nested_blockquote", "> outer quote\n>> inner quote\n> outer again\n\nAfter.".into()),
        ("quote_then_code", "> before\n\n> ```rust\n> let a = 1;\n> let b = 2;\n> ```\n\nAfter quote.".into()),
        (
            "mixed_document",
            "# Mixed\n\nParagraph with `inline code` and **bold** and [link](https://example.com).\n\n- item one\n- item two\n\n| H1 | H2 |\n|---|---|\n| a | b |\n\n> quoted\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\nFinal paragraph.\n".into(),
        ),
        (
            "emphasis_across_softbreak",
            "starts with open **emphasis\nand closes it here** end.\n\nNext paragraph.".into(),
        ),
        ("multiple_blank_separators", "para one\n\n\n\npara two\n".into()),
        ("list_then_code", "1. ordered item\n2. another\n\n```python\nx = 1\n```\n\nDone.".into()),
        ("trailing_blank_only", "just a paragraph\n\n\n".into()),
        ("heading_interrupts_para", "paragraph text\n## heading\n\nafter".into()),
        ("list_interrupts_para", "paragraph text\n- item one\n- item two\n\nafter".into()),
        ("code_then_para_no_blank", "```rust\nlet x = 1;\n```\npara directly after\n\nmore".into()),
        ("hr_after_para", "para line\n***\n\nafter".into()),
        ("hr_alone", "before\n\n---\n\nafter".into()),
        ("indented_code", "before\n\n    indented code line\n    another line\n\nafter".into()),
        ("ordered_loose_list", "1. one\n\n2. two\n\n3. three\n\nafter".into()),
        ("empty_fence", "```\n```\n\nafter".into()),
        ("long_code_block", "```rust\nlet a = 1;\nlet b = 2;\nlet c = 3;\nlet d = 4;\nlet e = 5;\n```\n\nDone.".into()),
        ("overwide_code_line", "```rust\nlet some_extremely_long_variable_name_that_exceeds_terminal_width_by_a_lot = 1234567890;\n```\n\nafter".into()),
        ("cjk_code_comment", "```rust\n// 中文注释的代码行，验证宽度与硬折行\nlet x = 1;\n```\n\n结束。".into()),
        // --- review-fix shapes (P1-4/P1-5) ---
        ("diff_block", "```diff\ndiff --git a/foo.rs b/foo.rs\nindex abc123..def456 100644\n--- a/foo.rs\n+++ b/foo.rs\n@@ -1,3 +1,4 @@\n-old line\n+new line\n context line\n```\n\nafter".into()),
        ("diff_block_no_git", "```diff\n--- a/bar.rs\n+++ b/bar.rs\n@@ -1 +1 @@\n-x\n+y\n```\n\nafter".into()),
        ("unclosed_diff", "intro\n\n```diff\ndiff --git a/bar.rs b/bar.rs\n@@ -1 +1 @@\n-x\n+y\n".into()),
        ("raw_html_block", "<div>\nraw html\n</div>\n\nafter".into()),
        ("html_then_para_inline", "text\n<b>inline html</b>\nmore\n\nafter".into()),
        ("html_then_code", "<div>\nx\n</div>\n\n```rust\nlet a = 1;\n```\n\nafter".into()),
        ("empty_list_item", "- \n\ntext after\n".into()),
        ("empty_list_item_then_more", "- \n\ntext after\n\nmore para\n".into()),
        ("empty_quote", "> \n\nafter".into()),
        ("quote_para_then_quote_code", "> before\n\n> ```rust\n> let a = 1;\n> ```\n\nafter".into()),
    ]
}

// ============================================================
// Span-level comparison
// ============================================================

fn span_pairs(lines: &[Line<'static>]) -> Vec<(String, Style)> {
    lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s: &Span| (s.content.to_string(), s.style))
        .collect()
}

fn describe(pairs: &[(String, Style)]) -> String {
    let mut out = String::new();
    for (text, style) in pairs {
        let _ = writeln!(out, "  [{text:?}] fg={:?}", style.fg);
    }
    out
}

fn reconcile(name: &str, corpus: &str, chunks: &[&str], width: u16, profile: Profile, extra: &str) {
    let palette = ThemePalette::default();
    let mut sr = StreamingRender::new(profile);
    for chunk in chunks {
        sr.push(chunk);
        // Sync every chunk — exactly what the UI does per frame.
        let _ = sr.lines(width, &palette);
    }
    let streaming: Vec<Line<'static>> = sr.lines(width, &palette).to_vec();
    let reference = full_lines(corpus, width, profile, &palette);
    pretty_assertions::assert_eq!(
        span_pairs(&streaming),
        span_pairs(&reference),
        "profile={profile:?} shape={name} width={width} {extra}\nstreaming:\n{}\nfull:\n{}",
        describe(&span_pairs(&streaming)),
        describe(&span_pairs(&reference)),
    );
}

// ============================================================
// The matrix
// ============================================================

const CHUNK_SIZES: &[usize] = &[1, 16, 256];
const WIDTHS: &[u16] = &[40, 80, 120];
const PROFILES: &[Profile] = &[Profile::Thinking, Profile::Content];

#[test]
fn reconcile_matrix_shapes() {
    for (name, corpus) in shapes() {
        for &profile in PROFILES {
            for &chunk in CHUNK_SIZES {
                for &width in WIDTHS {
                    let chunks = chunk_stream(&corpus, chunk);
                    reconcile(
                        name,
                        &corpus,
                        &chunks,
                        width,
                        profile,
                        &format!("chunk={chunk}B"),
                    );
                }
            }
            // Seeded random chunk sizes exercise arbitrary split points.
            for &seed in &[1u64, 2, 3] {
                for &width in WIDTHS {
                    let chunks = random_chunks(&corpus, seed, 97);
                    reconcile(
                        name,
                        &corpus,
                        &chunks,
                        width,
                        profile,
                        &format!("random-seed={seed}"),
                    );
                }
            }
        }
    }
}

/// Reconcile the full generated corpora (streamed at coarser chunks to
/// keep runtime sane) — the bench workloads themselves must converge.
#[test]
fn reconcile_matrix_corpora() {
    for scenario in common::SCENARIOS {
        let corpus = common::corpus(scenario, 4 * 1024);
        for &profile in PROFILES {
            for &chunk in &[16usize, 256] {
                for &width in WIDTHS {
                    let chunks = chunk_stream(&corpus, chunk);
                    reconcile(
                        scenario,
                        &corpus,
                        &chunks,
                        width,
                        profile,
                        &format!("corpus chunk={chunk}B"),
                    );
                }
            }
        }
    }
}

/// Width changes mid-stream must converge to the reference at the new
/// width (full rebuild), and the final width's output must be exact.
#[test]
fn reconcile_width_changes() {
    let palette = ThemePalette::default();
    for (name, corpus) in shapes() {
        for &profile in PROFILES {
            let mut sr = StreamingRender::new(profile);
            let chunks = chunk_stream(&corpus, 16);
            for (i, chunk) in chunks.iter().enumerate() {
                sr.push(chunk);
                // Oscillate widths across chunks.
                let width = match i % 3 {
                    0 => 40,
                    1 => 120,
                    _ => 80,
                };
                let _ = sr.lines(width, &palette);
            }
            let final_width = 80u16;
            let streaming: Vec<Line<'static>> = sr.lines(final_width, &palette).to_vec();
            let reference = full_lines(&corpus, final_width, profile, &palette);
            pretty_assertions::assert_eq!(
                span_pairs(&streaming),
                span_pairs(&reference),
                "width-oscillation profile={profile:?} shape={name}",
            );
        }
    }
}

/// finalize() replaces the incremental state with the reference render —
/// any transient drift converges.
#[test]
fn reconcile_finalize() {
    let palette = ThemePalette::default();
    for (name, corpus) in shapes() {
        for &profile in PROFILES {
            let mut sr = StreamingRender::new(profile);
            for chunk in chunk_stream(&corpus, 16) {
                sr.push(chunk);
                let _ = sr.lines(80, &palette);
            }
            sr.finalize(80, &palette);
            let reference = full_lines(&corpus, 80, profile, &palette);
            pretty_assertions::assert_eq!(
                span_pairs(sr.lines(80, &palette)),
                span_pairs(&reference),
                "finalize profile={profile:?} shape={name}",
            );
        }
    }
}
