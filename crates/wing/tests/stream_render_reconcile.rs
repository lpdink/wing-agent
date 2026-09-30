//! Span-exactness reconciliation matrix for streaming rendering.
//!
//! For every (corpus shape × chunk size × width) combination, the
//! incremental `StreamingRender` output must equal the reference full
//! render ([`full_lines`]) span by span (text + style) — for BOTH profiles
//! (reasoning and assistant content differ in the parse rules `Profile`
//! owns, not in the reference they reconcile against).
//!
//! The same holds with image anchors enabled: the incremental engine's
//! resting state must agree with the reference render on the **anchors** too
//! (`line`/`column`/`cols`/`rows`, i.e. the geometry the drawing layer
//! consumes), not just on the text and the link spans.
//!
//! Scope of the assertion: the **last frame before `finalize()`**, i.e.
//! the state the incremental engine leaves after every chunk has been
//! pushed and synced. Transient mid-stream deviations are allowed by
//! design (a misjudged block boundary only delays promotion) and are
//! reconciled at turn end; what must hold is that the stream's resting
//! state never diverges from the reference without converging later.
//!
//! Deliberately NOT in `shapes()` (see the module header's "Known limit"):
//! a reference-style link definition in one block and its use in another —
//! slice-isolated parsing cannot resolve it, so it renders literally until
//! `finalize()`. Adding it here would assert something the engine does not
//! promise.

mod common;

use std::fmt::Write as _;

use common::{chunk_stream, random_chunks};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use wing::config::ThemePalette;
use wing::render::markdown::ImageEntry;
use wing::render::markdown::ImageOpts;
use wing::render::markdown::ImageShape;
use wing::render::markdown::Profile;
use wing::render::markdown::stream::{StreamingRender, full_lines, full_render};

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
        // --- review round 2 ---
        ("indented_closing_fence", "```rust\nlet x = 1;\n  ```\n\nafter the block\n".into()),
        ("indented_closing_fence_trailing_space", "```rust\nlet x = 1;\n  ```  \n\nafter\n".into()),
        ("indented_closing_fence_tilde", "~~~python\nx = 1\n   ~~~\n\nafter\n".into()),
        (
            "crlf_document",
            "First paragraph.\r\n\r\n```rust\r\nlet a = 1;\r\nlet b = 2;\r\n```\r\n\r\nafter\r\n".into(),
        ),
        (
            "crlf_unclosed_fence",
            "intro\r\n\r\n```rust\r\nlet x = 1;\r\nlet y = 2;\r\n".into(),
        ),
        // --- streaming/reference reconciliations found on real sessions ---
        // A blank line right before the closing fence is dropped by the
        // reference (its body is `trim_end_matches('\n')`d); the code cache
        // must hold it as provisional until a non-empty line follows, or a
        // chunk boundary between the blank line and the closer leaves an
        // extra body line in the resting state.
        ("fence_body_ends_blank", "```rust\nlet x = 1;\n\n```\n\nafter\n".into()),
        (
            "fence_body_ends_blanks",
            "intro\n\n```\nline one\n\n\n```\n\nafter\n".into(),
        ),
        // A BARE fence that interrupts a list slice: the fence line is the
        // opener and must not be reprocessed as its own closer (it has no
        // info string, so a closer check matches it), or the splitter
        // promotes an empty block and renders the body as prose.
        (
            "list_then_bare_fence",
            "- item one\n- item two\n\n```\nlet x = 1;\n```\n\nafter the block\n".into(),
        ),
        (
            "list_immediately_then_bare_fence",
            "- item one\n- item two\n```\nlet x = 1;\n```\n\nafter\n".into(),
        ),
        // An indented block that RESOLVES to nothing (an empty list item):
        // the tail separator before it must not survive the collapse, and at
        // the top of a cell the collapsed block's own blank line keeps the
        // first-line prefix.
        ("indented_block_empty_item", "intro\n\n    1.\n\n    2.\n".into()),
        (
            "indented_block_empty_item_first",
            "    -  \n\npara\n".into(),
        ),
        // CRLF: `\r\n` endings trim like `\n` (pulldown hands the reference
        // an LF-normalized copy), and a bare `\r` is content — the reference
        // renders it.
        (
            "crlf_fence_body_ends_blank",
            "```rust\r\nlet x = 1;\r\n\r\n```\r\n\r\nafter\r\n".into(),
        ),
        (
            "crlf_partial_line_keeps_cr",
            "```rust\r\nlet x = 1;\r\nlet y = 2;\r".into(),
        ),
        // An indented fence belongs to the list item it sits in: cutting it
        // out of the item's slice loses the list continuation prefix.
        (
            "indented_fence_in_list",
            "- item one\n  ```rust\n  let x = 1;\n  ```\n\nafter\n".into(),
        ),
        // Image anchors: the `Off` matrix renders these through the link
        // path (an image is a link there — the pre-anchor behaviour), while
        // `reconcile_matrix_images` runs the same corpus with `Anchor` mode.
        (
            "images_link_path",
            "A paragraph with an image inline ![inline](./plots/a.png) and text after.\n\n![standalone](./plots/a.png)\n\n| chart | note |\n|---|---|\n| ![cell](./plots/a.png) | x |\n\n- ![listed](./plots/a.png)\n\n> ![quoted](./plots/a.png)\n".into(),
        ),
        // Indented blocks: code for content, prose for reasoning (see
        // `Profile`) — the nested re-parse must agree with the reference in
        // both profiles, including its markdown structure.
        (
            "indented_block_with_markdown",
            "note:\n\n    a nested **thought** with `code`\n\n    - bullet one\n    - bullet two\n\nafter\n".into(),
        ),
        (
            "indented_block_then_paragraph",
            "before\n\n    indented prose line\nimmediately after\n\nend\n".into(),
        ),
        // --- math (LaTeX) shapes ---
        // The formulas themselves must render (character grids, equal in both
        // profiles); everything the engine declines must stay the source, and
        // code / currency must not become math.
        (
            "math_inline",
            "Before $x^2 + y^2 = z^2$ and $\\alpha + \\beta$ and $\\mathbb{R}^n$ after.\n\nNext paragraph.".into(),
        ),
        (
            "math_inline_multiline_source",
            // `render_inline` refuses (the fraction is 3 rows): the source is
            // shown, the sentence keeps rendering around it.
            "A fraction $\\frac{a}{b}$ inline and $\\sum_{i=1}^{n} i$ too.\n\nafter".into(),
        ),
        (
            "math_display_multiline",
            "intro\n\n$$\n\\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}\n$$\n\nafter".into(),
        ),
        (
            "math_display_inline_in_paragraph",
            "the identity $$e^{i\\pi} + 1 = 0$$ is famous\n\nafter".into(),
        ),
        (
            "math_paren_bracket_delimiters",
            "inline \\(a^2 + b^2\\) and a display:\n\n\\[\nE = mc^2\n\\]\n\nafter".into(),
        ),
        (
            "math_bare_align",
            "The system:\n\n\\begin{align}\nf(x) &= x^2 + 2x + 1 \\\\\n     &= (x+1)^2\n\\end{align}\n\nafter".into(),
        ),
        (
            "math_cases_matrix",
            "$$\nf(x) = \\begin{cases}\n1 & x > 0 \\\\\n0 & x = 0\n\\end{cases}\n$$\n\n$$\n\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}\n$$\n\nafter".into(),
        ),
        (
            "math_degraded_source",
            "\\begin{tikzcd} a \\arrow[r] & b \\end{tikzcd}\n\n$\\ce{2H2O}$ and $\\dfrac{a}{b}$.\n\nafter".into(),
        ),
        (
            "math_currency_and_code",
            "it costs $100 and $200 total\n\n```sh\necho \"$HOME and $PATH\"\n```\n\ninline `$x$` and `\\(y\\)` stay code.\n\nafter".into(),
        ),
        (
            "math_overwide_display",
            format!("$$\n{}\n$$\n\nafter\n", "a".repeat(200)),
        ),
        (
            "math_unclosed_delimiters",
            "cost $x + 1\n\nstarts \\(y + 2 and never closes\n\nafter".into(),
        ),
        // The formula itself is the ACTIVE TAIL (no trailing block closes it):
        // this is the shape that exercises the incremental tail path with
        // math, not just the promoted-block path.
        (
            "math_display_tail",
            "intro\n\n$$\n\\begin{aligned}\na &= b \\\\\nc &= d\n\\end{aligned}\n$$".into(),
        ),
        ("math_inline_tail", "the answer is $x^2 + y^2$".into()),
        // --- review r1: regions pulldown does not parse as text ---
        (
            "math_link_destination",
            "see [a](http://x/\\(y\\)) and [b](http://x/\\(y\\) \"t \\(z\\)\") here\n\nafter".into(),
        ),
        (
            // NOTE: no `[ref]` USE — a reference-style link resolves at document
            // scope, so it renders literally while streaming (see the module
            // header). The destination itself is covered by the unit and
            // integration tests in `math.rs` / `math_render.rs`.
            "math_reference_definition",
            "[ref]: http://x/\\(y\\) \"title \\(z\\)\"\n\nuse \\(a\\)\n\nafter".into(),
        ),
        (
            "math_html_block",
            "<div>\n\\(x\\) and \\begin{align}a\\end{align}\n</div>\n\nafter \\(b\\)".into(),
        ),
        (
            "math_autolink",
            "link <http://x/\\(y\\)> and text \\(z\\)\n\nafter".into(),
        ),
        // --- review r1: code regions behind a block prefix (S2) ---
        (
            "math_quoted_tilde_fence",
            "> ~~~\n> \\begin{align}a\\end{align}\n> ~~~\n\nafter \\(x\\)\n".into(),
        ),
        (
            "math_quoted_fence_blank_inside",
            // The blank line is fence body; a slice cut there would render the
            // rest as prose (the splitter's `PrefixedFence` exists for this).
            "> ~~~\n> a\n\n> \\(x\\)\n> ~~~\n\nafter\n".into(),
        ),
        (
            "math_quoted_indented_code",
            ">     \\(x\\)\n\nafter".into(),
        ),
        (
            "math_list_fence",
            "- ```\n  \\(x\\)\n  ```\n\nafter\n".into(),
        ),
        // --- review r1: inline math must not break prose wrapping (B1) ---
        (
            "math_inline_long_paragraph",
            "the quick brown fox $x^2$ jumps over the lazy dog and then keeps running far \
             beyond the right margin of a narrow cell so we can see how wrapping behaves \
             when a formula sits in the middle of prose, 并且中文也需要在边界处折行。\n\nafter".into(),
        ),
        (
            "math_inline_overwide",
            format!("Sum: $a_1{} end\n\nafter\n", " + a_2 + a_3 + a_4 + a_5 + a_6".repeat(3)),
        ),
        // --- review r2: tabs (the panic) and prefixed-fence closure ---
        (
            // A tab is four COLUMNS but one byte: the shape helpers must not
            // slice at a column count (this input used to panic).
            "math_tab_indented_lines",
            "\t- 中文项目\n\n\tx\n\n> \t\n\npara\n\t🙂x\n".into(),
        ),
        (
            // A prefix-less fence line after a `> ~~~` fence is a NEW top-level
            // fence (CommonMark), not that fence's closer.
            "math_quoted_fence_bare_closer",
            "> ~~~\n> a\n~~~\n\nafter\n".into(),
        ),
        (
            "math_quoted_fence_bare_closer_no_blank",
            "> ~~~\n> a\n~~~\nafter\n".into(),
        ),
        (
            "math_quoted_fence_bare_closer_after_blank",
            "> ~~~\n> a\n\n~~~   \n\nafter\n".into(),
        ),
        (
            "math_quoted_fence_bare_closer_after_quoted",
            "> ~~~\n> a\n\n> b\n~~~\n\nafter\n".into(),
        ),
        // NOTE: the backtick variant of the shape above is NOT here — a quoted
        // backtick fence goes through `ensure_fences_on_own_line`, whose
        // insertion lands differently depending on the chunk boundary. Both
        // this build and the `a946327` baseline diverge from the reference for
        // one chunk size (5), so it is pre-existing noise, registered in
        // `docs/dev/tui-rendering.md` §4 instead of asserted here.
        (
            "math_list_fence_prefixed_closer",
            "- ~~~\n  a\n  ~~~\n\nafter\n".into(),
        ),
        (
            "math_list_fence_bare_line",
            "- ~~~\n  a\n~~~\n\nafter\n".into(),
        ),
        (
            // The destination on the line after the definition.
            "math_reference_definition_wrapped",
            "[a b]:\n  http://x/\\(y\\) \"t \\(z\\)\"\n\nuse \\(a\\)\n\nafter\n".into(),
        ),
        // --- review r3: the swallowed content carries a formula, so a wrong
        // fence model would show up as a code-content rewrite (resting state
        // != reference) instead of staying invisible ---
        (
            "math_bare_closer_swallows_formula",
            "> ~~~\n> a\n~~~\n\n\\(x\\) after\n".into(),
        ),
        (
            "math_bare_closer_swallows_formula_after_blank",
            "> ~~~\n> a\n\n~~~\n\n\\(x\\) after\n".into(),
        ),
        (
            "math_list_fence_interrupted_by_quote",
            // The item's content is interrupted inside the fence, so the
            // parser re-reads the indented fence line as a NEW top-level fence.
            "- ~~~\n> \n  ~~~\n\n\\[z\\] after\n".into(),
        ),
        (
            "math_list_fence_closed_then_formula",
            // An intact item: the closer really ends the fence, so what
            // follows is prose and gets rewritten.
            "- ~~~\n  a\n  ~~~\n\n\\(x\\) after\n".into(),
        ),
        (
            "math_list_item_fence_with_blanks",
            "- ```\n  \n  \n\n\n  ```\n```\n\n\\(x\\) after\ntext \\(a\\)\n".into(),
        ),
        (
            // NOTE: `  - ``` … > ``` ` (an item fence followed by a quote +
            // backtick fence) is NOT here: the backtick fence goes through
            // `ensure_fences_on_own_line`, whose insertion lands differently
            // depending on the chunk boundary — both this build and the
            // `a946327` baseline diverge from the reference for some chunk
            // sizes (different ones), so it is pre-existing noise registered
            // in `docs/dev/tui-rendering.md` §4.
            "math_list_fence_then_top_fence",
            // Tilde fences: the backtick variants of these container shapes go
            // through `ensure_fences_on_own_line` (see the NOTE above).
            "- ~~~\n  a\n~~~\n\n\\(x\\) after\n".into(),
        ),
        (
            "math_list_continuation_fence",
            "- item\n  ~~~\n  body\n  ~~~\n\n\\(x\\) after\n".into(),
        ),
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
    reconcile_with(
        name,
        corpus,
        chunks,
        width,
        profile,
        ImageOpts::default(),
        extra,
    );
}

/// [`reconcile`] with image anchors configured: text, link spans **and**
/// anchor geometry must all match the reference render.
fn reconcile_with(
    name: &str,
    corpus: &str,
    chunks: &[&str],
    width: u16,
    profile: Profile,
    images: ImageOpts,
    extra: &str,
) {
    let palette = ThemePalette::default();
    let mut sr = StreamingRender::with_images(profile, images.clone());
    for chunk in chunks {
        sr.push(chunk);
        // Sync every chunk — exactly what the UI does per frame.
        let _ = sr.lines(width, &palette);
    }
    let rendered = sr.composed(width, &palette);
    let streaming: Vec<Line<'static>> = rendered.lines.to_vec();
    let streaming_links = rendered.links.to_vec();
    let streaming_images = rendered.images.to_vec();
    let reference = full_render(corpus, width, profile, &palette, &images);
    let (reference_lines, reference_links, reference_images) = reference.into_parts();
    pretty_assertions::assert_eq!(
        span_pairs(&streaming),
        span_pairs(&reference_lines),
        "profile={profile:?} shape={name} width={width} {extra}\nstreaming:\n{}\nfull:\n{}",
        describe(&span_pairs(&streaming)),
        describe(&span_pairs(&reference_lines)),
    );
    pretty_assertions::assert_eq!(
        streaming_links,
        reference_links,
        "links profile={profile:?} shape={name} width={width} {extra}"
    );
    pretty_assertions::assert_eq!(
        streaming_images,
        reference_images,
        "anchors profile={profile:?} shape={name} width={width} {extra}"
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

/// Math shapes across EVERY chunk size in 1..=64.
///
/// A `$` / `\(` / `\begin{` split at any byte is the case the incremental
/// engine is most likely to get wrong (the delimiter normalization depends on
/// where the slice boundary fell), so the math shapes get the full chunk
/// sweep instead of the matrix's three sizes.
#[test]
fn reconcile_math_shapes_every_chunk_size() {
    let math_shapes: Vec<(&'static str, String)> = shapes()
        .into_iter()
        .filter(|(name, _)| name.starts_with("math_"))
        .collect();
    assert!(
        math_shapes.len() >= 8,
        "math shapes missing from `shapes()`: {}",
        math_shapes.len()
    );
    for (name, corpus) in math_shapes {
        for &profile in PROFILES {
            for chunk in 1..=64usize {
                let chunks = chunk_stream(&corpus, chunk);
                reconcile(
                    name,
                    &corpus,
                    &chunks,
                    80,
                    profile,
                    &format!("chunk={chunk}B"),
                );
            }
        }
    }
}

// ============================================================
// Image anchors
// ============================================================

/// Corpus shapes with anchors actually produced (standalone images with
/// metadata), plus the shapes that must keep the link path under the same
/// options.
fn image_shapes() -> Vec<(&'static str, String)> {
    vec![
        (
            "image_standalone",
            "before the image\n\n![销售趋势](./plots/a.png)\n\nafter the image\n".into(),
        ),
        (
            "image_two_anchors",
            "![one](./plots/a.png)\n\na paragraph\n\n![two](./plots/b.png)\n".into(),
        ),
        (
            "image_wide_and_tall",
            "![wide](./plots/wide.png)\n\n![tall](./plots/tall.png)\n".into(),
        ),
        (
            "image_between_blocks",
            "para\n\n![a](./plots/a.png)\n\n```rust\nlet x = 1;\n```\n\n![b](./plots/a.png)\n\nafter\n".into(),
        ),
        (
            "image_in_the_middle_of_a_paragraph",
            "text ![inline](./plots/a.png) tail\n\n![standalone](./plots/a.png)\n".into(),
        ),
        (
            "image_in_fence_and_list",
            "```markdown\n![in fence](./plots/a.png)\n```\n\n- ![listed](./plots/a.png)\n\n> ![quoted](./plots/a.png)\n".into(),
        ),
        (
            "image_unknown_path",
            "![missing](./plots/missing.png)\n\n![known](./plots/a.png)\n".into(),
        ),
        (
            "image_trailing",
            "intro\n\n![last](./plots/a.png)".into(),
        ),
        (
            "image_at_the_top",
            "![first](./plots/a.png)\n\ntext after\n".into(),
        ),
        (
            "image_then_heading",
            "![a](./plots/a.png)\n\n# Heading\n\nbody\n".into(),
        ),
    ]
}

/// The metadata table the image matrix renders with: `/ws` is the workspace,
/// `plots/a.png` is a 4:3 image (30 rows at width 80) and the extremes cover
/// the cap and the floor.
fn image_opts() -> ImageOpts {
    let root = std::path::PathBuf::from("/ws");
    ImageOpts::anchor(
        Some(root.clone()),
        vec![
            ImageEntry::new(root.join("plots/a.png"), ImageShape::new(800, 600)),
            ImageEntry::new(root.join("plots/b.png"), ImageShape::new(1600, 900)),
            ImageEntry::new(root.join("plots/wide.png"), ImageShape::new(2000, 100)),
            ImageEntry::new(root.join("plots/tall.png"), ImageShape::new(300, 4000)),
        ],
    )
}

/// Narrow widths (1..=6 columns).
///
/// The main matrix starts at 40 columns, which is where a chat cell normally
/// lives — but nothing in the layout enforces a minimum width (a tmux pane can
/// be one column wide), and the hard wrap is at its most aggressive there: a
/// 2-column cell prefix alone fills a 1-column row, so *ordinary* rows end up
/// looking exactly like an anchor's blank cover rows. That is what broke the
/// blank-line dedup at width 1 (the streaming resting state grew a blank line
/// the reference render does not have), so the narrow band is asserted
/// explicitly, for every shape and with anchors on and off.
#[test]
fn reconcile_matrix_narrow_widths() {
    const NARROW: &[u16] = &[1, 2, 3, 4, 5, 6];
    let images = image_opts();
    // Not vacuous on either side of the band: width 1 cannot hold an anchor
    // (the markdown width is the cell width minus the 2-column prefix), while
    // the top of the band still produces them.
    assert!(
        !full_render(
            "![a](./plots/a.png)",
            1,
            Profile::Content,
            &ThemePalette::default(),
            &images
        )
        .has_images(),
        "width 1 must not produce anchors"
    );
    assert!(
        full_render(
            "![a](./plots/a.png)",
            6,
            Profile::Content,
            &ThemePalette::default(),
            &images
        )
        .has_images(),
        "the narrow band must still exercise anchors"
    );
    for (name, corpus) in shapes().into_iter().chain(image_shapes()) {
        for &profile in PROFILES {
            for &width in NARROW {
                for &chunk in &[1usize, 16] {
                    let chunks = chunk_stream(&corpus, chunk);
                    let extra = format!("narrow chunk={chunk}B width={width}");
                    reconcile(name, &corpus, &chunks, width, profile, &extra);
                    reconcile_with(
                        name,
                        &corpus,
                        &chunks,
                        width,
                        profile,
                        images.clone(),
                        &format!("{extra} images=anchor"),
                    );
                }
            }
        }
    }
}

/// With anchors enabled, the incremental engine must converge to the
/// reference render including the anchor geometry — every chunk size, width
/// and profile.
#[test]
fn reconcile_matrix_images() {
    let images = image_opts();
    for (name, corpus) in image_shapes() {
        for &profile in PROFILES {
            for &chunk in CHUNK_SIZES {
                for &width in WIDTHS {
                    let chunks = chunk_stream(&corpus, chunk);
                    reconcile_with(
                        name,
                        &corpus,
                        &chunks,
                        width,
                        profile,
                        images.clone(),
                        &format!("chunk={chunk}B images=anchor"),
                    );
                }
            }
            for &seed in &[1u64, 2, 3] {
                for &width in WIDTHS {
                    let chunks = random_chunks(&corpus, seed, 97);
                    reconcile_with(
                        name,
                        &corpus,
                        &chunks,
                        width,
                        profile,
                        images.clone(),
                        &format!("random-seed={seed} images=anchor"),
                    );
                }
            }
        }
    }
    // The image shapes must also hold on the link path (the same corpus with
    // images off), and they must actually produce anchors in the anchoring
    // runs — otherwise the matrix above is vacuous.
    for (name, corpus) in image_shapes() {
        let chunks = chunk_stream(&corpus, 16);
        reconcile(name, &corpus, &chunks, 80, Profile::Content, "images=off");
    }
    let anchored: usize = full_render(
        &image_shapes()
            .iter()
            .map(|(_, corpus)| corpus.clone())
            .collect::<String>(),
        80,
        Profile::Content,
        &ThemePalette::default(),
        &image_opts(),
    )
    .images()
    .iter()
    .flatten()
    .count();
    assert!(
        anchored >= 8,
        "the image corpus must produce anchors, got {anchored}"
    );
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

/// Prefix-exactness: for the shapes whose output converges mid-stream,
/// EVERY prefix (not just the last frame) must equal the reference render
/// of the same prefix. This is the property that catches cursor/offset
/// drift in the incremental paths — a stale or duplicated body line shows
/// up here immediately, where the end-of-stream compare would only notice
/// if it survived to the final frame.
///
/// Shapes whose mid-stream output is transiently different by design (diff
/// fences' whole-block metadata handling, raw HTML blocks, empty list
/// items) are excluded: they converge at `finalize()`, not per prefix.
#[test]
fn reconcile_prefixes() {
    const CONVERGENT: &[&str] = &[
        "plain_paragraphs",
        "cjk_prose",
        "unclosed_fence",
        "fence_with_blank_lines",
        "long_code_block",
        "overwide_code_line",
        "tight_list",
        "ordered_loose_list",
        "table",
        "mixed_document",
        "indented_code",
        "indented_closing_fence",
        "indented_closing_fence_trailing_space",
        "indented_closing_fence_tilde",
        "crlf_document",
        "crlf_unclosed_fence",
        // Math converges per prefix too: an unterminated `\(` is literal in
        // both the streamed prefix and the reference of that prefix, and the
        // rewrite only fires once the closing delimiter has arrived.
        "math_inline",
        "math_inline_multiline_source",
        "math_display_multiline",
        "math_display_inline_in_paragraph",
        "math_paren_bracket_delimiters",
        "math_bare_align",
        "math_currency_and_code",
        "math_unclosed_delimiters",
        "math_display_tail",
        "math_inline_tail",
        "math_link_destination",
        "math_reference_definition",
        "math_quoted_tilde_fence",
        "math_quoted_fence_blank_inside",
        "math_quoted_indented_code",
        "math_list_fence",
        "math_inline_long_paragraph",
        "math_inline_overwide",
        "math_tab_indented_lines",
        "math_quoted_fence_bare_closer",
        "math_quoted_fence_bare_closer_no_blank",
        "math_quoted_fence_bare_closer_after_blank",
        "math_quoted_fence_bare_closer_after_quoted",
        "math_quoted_backtick_fence_bare_closer",
        "math_list_fence_prefixed_closer",
        "math_list_fence_bare_line",
        "math_reference_definition_wrapped",
        "math_bare_closer_swallows_formula",
        "math_bare_closer_swallows_formula_after_blank",
        "math_list_fence_interrupted_by_quote",
        "math_list_fence_closed_then_formula",
        "math_list_item_fence_with_blanks",
        "math_list_fence_then_top_fence",
        "math_list_continuation_fence",
    ];
    let palette = ThemePalette::default();
    for (name, corpus) in shapes() {
        if !CONVERGENT.contains(&name) {
            continue;
        }
        for &profile in PROFILES {
            for &width in &[40u16, 80] {
                let mut sr = StreamingRender::new(profile);
                let mut fed = String::new();
                for chunk in chunk_stream(&corpus, 7) {
                    sr.push(chunk);
                    fed.push_str(chunk);
                    let streamed = sr.lines(width, &palette).to_vec();
                    let reference = full_lines(&fed, width, profile, &palette);
                    pretty_assertions::assert_eq!(
                        span_pairs(&streamed),
                        span_pairs(&reference),
                        "prefix shape={name} profile={profile:?} width={width} \
                         at {} bytes:\nstreaming:\n{}\nreference:\n{}",
                        fed.len(),
                        describe(&span_pairs(&streamed)),
                        describe(&span_pairs(&reference)),
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
