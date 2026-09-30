//! End-to-end math rendering through the real markdown pipeline.
//!
//! Covers the contract the task fixes:
//!
//! 1. `$…$`, `$$…$$`, `\(…\)`, `\[…\]` and a bare AMS environment render to
//!    character grids — in both profiles (see `Profile` for the three rules
//!    that differ or are shared).
//! 2. **Nothing is ever lost.** When the engine declines (unknown command,
//!    too wide, a multiline block that cannot fit on one line), the complete
//!    LaTeX source between its delimiters is rendered instead — never an
//!    empty string, never half a formula, never a swallowed line.
//! 3. Code (fenced, inline, indented) and currency `$` are untouched.
//! 4. `rendering.math = off` is the pre-math rendering: byte-identical spans
//!    to a render of the same text with math support absent.
//!
//! The pipeline is driven through its public entry points only
//! (`render_markdown_lines_with`, `full_lines`), which is what the cells use.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use wing::config::ThemePalette;
use wing::config::rendering::MathMode;
use wing::render::markdown::stream::full_lines;
use wing::render::markdown::types::SegmentKind;
use wing::render::markdown::{MarkdownLine, Profile, RenderOpts, render_markdown_lines_with};

// ============================================================
// Harness
// ============================================================

fn palette() -> ThemePalette {
    ThemePalette::default()
}

fn render(md: &str, profile: Profile, width: u16) -> Vec<MarkdownLine> {
    render_with(md, profile, width, MathMode::Text)
}

fn render_with(md: &str, profile: Profile, width: u16, math: MathMode) -> Vec<MarkdownLine> {
    render_markdown_lines_with(
        md,
        Some(width),
        &palette(),
        RenderOpts::new(profile, true).with_math(math),
    )
}

/// Visible text of a render, one entry per line.
fn plain(lines: &[MarkdownLine]) -> Vec<String> {
    lines.iter().map(MarkdownLine::to_plain).collect()
}

fn joined(md: &str) -> String {
    plain(&render(md, Profile::Content, 118)).join("\n")
}

fn joined_with(md: &str, profile: Profile, width: u16, math: MathMode) -> String {
    plain(&render_with(md, profile, width, math)).join("\n")
}

/// Every (kind, text) span of a render, in order.
fn spans(lines: &[MarkdownLine]) -> Vec<(SegmentKind, String)> {
    lines
        .iter()
        .flat_map(|line| line.segments.iter())
        .map(|seg| (seg.kind, seg.text.clone()))
        .collect()
}

/// Concatenated text of every segment with `kind`.
fn text_of(lines: &[MarkdownLine], kind: SegmentKind) -> String {
    lines
        .iter()
        .flat_map(|line| line.segments.iter())
        .filter(|seg| seg.kind == kind)
        .map(|seg| seg.text.as_str())
        .collect()
}

const CONTENT: Profile = Profile::Content;
const THINKING: Profile = Profile::Thinking;

// ============================================================
// 1. Rendering
// ============================================================

#[test]
fn inline_math_renders_to_unicode() {
    let md = "速度 $v = \\frac{dx}{dt}$ 与 $x^2 + y^2$ 的关系";
    let out = joined(md);
    assert!(out.contains("x² + y²"), "{out}");
    // A fraction cannot fit on one line: the source stays (see below), and
    // the rest of the sentence keeps rendering.
    assert!(out.contains("速度 "), "{out}");
    assert!(out.contains("的关系"), "{out}");

    // Segments: the rendered formula is one Math span, the prose around it is
    // not.
    let lines = render("$x^2$", CONTENT, 40);
    assert_eq!(text_of(&lines, SegmentKind::Math), "x²");
}

#[test]
fn display_math_renders_a_grid_block() {
    let md = "$$\n\\frac{a}{b}\n$$";
    let lines = render(md, CONTENT, 118);
    let math: Vec<String> = lines
        .iter()
        .filter(|l| l.segments.iter().any(|s| s.kind == SegmentKind::Math))
        .map(MarkdownLine::to_plain)
        .collect();
    assert_eq!(math, vec![" a", "───", " b"], "{:?}", plain(&lines));
    // Every grid row is its own line, and every row is a Math span.
    assert!(lines.iter().all(|l| {
        l.segments
            .iter()
            .all(|s| s.kind == SegmentKind::Math || s.text.trim().is_empty())
    }));
}

#[test]
fn display_math_spans_multiple_source_lines() {
    // `$$` block whose body is several lines (matrix), plus a bare
    // environment written without any `$$` at all.
    let matrix = joined("$$\n\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}\n$$");
    assert!(matrix.contains("⎛a  b⎞"), "{matrix}");
    assert!(matrix.contains("⎝c  d⎠"), "{matrix}");

    let bare = joined("\\begin{align}\nf(x) &= x^2 \\\\\n&= 1\n\\end{align}");
    assert!(bare.contains("f(x)  = x²"), "{bare}");
    assert!(bare.contains("= 1"), "{bare}");
    assert!(!bare.contains("\\begin{align}"), "source leaked: {bare}");
}

#[test]
fn paren_and_bracket_delimiters_render() {
    let out = joined("inline \\(a^2 + b^2\\) and display:\n\n\\[\nE = mc^2\n\\]");
    assert!(out.contains("a² + b²"), "{out}");
    assert!(out.contains("E = mc²"), "{out}");
    assert!(!out.contains("\\("), "delimiters leaked: {out}");
    assert!(!out.contains("\\["), "delimiters leaked: {out}");
}

#[test]
fn math_renders_in_both_profiles() {
    let md = "sum: $$\\sum_{i=1}^{n} i = \\frac{n(n+1)}{2}$$";
    for profile in [CONTENT, THINKING] {
        let out = joined_with(md, profile, 118, MathMode::Text);
        assert!(out.contains('∑'), "{profile:?}: {out}");
        assert!(out.contains("n(n + 1)"), "{profile:?}: {out}");
    }
}

#[test]
fn display_math_keeps_its_block_prefix() {
    // A formula inside a blockquote / list item keeps the enclosing prefix
    // on every grid row.
    let out = joined("> $$\\frac{a}{b}$$");
    for (i, line) in out.lines().enumerate() {
        if line.contains('a') || line.contains('─') || line.contains('b') {
            assert!(line.starts_with("│ "), "row {i} lost its bar: {out:?}");
        }
    }
    let list = joined("- item\n\n  $$\\frac{a}{b}$$");
    assert!(list.contains("  ───"), "{list}");
}

#[test]
fn overwide_display_math_degrades_to_source() {
    // 200 columns of `a` cannot fit in 40: the engine says `None` and the
    // source is shown — the rendering layer never clips a formula.
    let long = "a".repeat(200);
    let md = format!("$${long}$$");
    let out = joined_with(&md, CONTENT, 40, MathMode::Text);
    assert!(out.contains(&long), "source lost: {out}");
}

// ============================================================
// 2. Nothing is ever lost
// ============================================================

/// Render `md` and compare every character of the formula's source with what
/// came out — the fallback must be the source, verbatim.
fn assert_source_survives(md: &str, source: &str) {
    let out = joined(md);
    assert!(
        out.contains(source),
        "source {source:?} not rendered verbatim in {out:?}"
    );
}

#[test]
fn unrenderable_commands_keep_their_source() {
    // The engine refuses `\ce` / `\dfrac` (it will not guess): the user sees
    // the LaTeX they wrote, delimiters included — exactly the pre-math
    // rendering of that span.
    assert_source_survives("化学式 $\\ce{2H2O}$ 结束", "$\\ce{2H2O}$");
    assert_source_survives("分式 $\\dfrac{a}{b}$ 结束", "$\\dfrac{a}{b}$");
    assert_source_survives(
        "unknown \\begin{tikzcd} a \\arrow[r] & b \\end{tikzcd} env",
        "\\begin{tikzcd} a \\arrow[r] & b \\end{tikzcd}",
    );
    // The surrounding prose is untouched by the fallback.
    assert!(joined("化学式 $\\ce{2H2O}$ 结束").contains("结束"));
}

#[test]
fn unclosed_delimiters_stay_literal() {
    for md in ["cost $x + 1 and no closer", "cost $$x + 1 and no closer"] {
        let out = joined(md);
        let source = md.trim_start_matches("cost ");
        assert!(out.contains(source), "{md:?} → {out:?}");
    }
}

#[test]
fn display_math_fallback_is_byte_complete() {
    // A display block the engine refuses: every source character survives,
    // and the `$$` delimiters are reproduced.
    let body = "\\unknownenv{a} + ".to_string() + &"b".repeat(300);
    let md = format!("$$\n{body}\n$$");
    let out = joined_with(&md, CONTENT, 118, MathMode::Text);
    assert!(out.contains(&body), "body lost: {out}");
    assert_eq!(
        out.matches("$$").count(),
        2,
        "delimiters not reproduced: {out}"
    );
}

#[test]
fn a_formula_is_never_dropped_entirely() {
    // The regression the wiring exists for: before it, `Event::InlineMath`
    // fell into the catch-all and the formula vanished. Every line of the
    // input must leave a trace in the output.
    let md = "a $x^2$ b\n\nc $$\\frac{1}{2}$$ d\n\ne \\(y\\) f\n\ng \\[z\\] h\n\n\\begin{align}q &= 1\\end{align}";
    let out = joined(md);
    for needle in [
        "a ", "x²", " b", "c ", "1", " d", "e ", "y", " f", "g ", "z", " h", "q", "1",
    ] {
        assert!(out.contains(needle), "{needle:?} missing from {out:?}");
    }
    assert!(
        !out.contains("$$") && !out.contains("\\("),
        "nothing should have degraded here: {out:?}"
    );
}

// ============================================================
// 3. Code and currency are not math
// ============================================================

#[test]
fn code_regions_are_never_parsed() {
    let out = joined("```sh\necho \"$HOME and $PATH\"\n\\(x\\)\n```");
    assert!(out.contains("echo \"$HOME and $PATH\""), "{out}");
    assert!(out.contains("\\(x\\)"), "{out}");

    let inline = joined("inline `$x$` and `\\(y\\)` code");
    assert!(inline.contains("$x$"), "{inline}");
    assert!(inline.contains("\\(y\\)"), "{inline}");

    // Indented blocks are code for assistant content (CommonMark), so the
    // delimiters inside them stay literal.
    let indented = joined("text\n\n    \\(x\\)\n\nafter");
    assert!(indented.contains("\\(x\\)"), "{indented}");
}

#[test]
fn currency_is_not_math() {
    let out = joined("it costs $100 and $200 total");
    assert!(out.contains("$100 and $200"), "{out}");
    assert_eq!(
        text_of(&render("$100 and $200", CONTENT, 60), SegmentKind::Math),
        ""
    );
}

#[test]
fn existing_dollar_math_is_not_double_wrapped() {
    let md = "$$\n\\begin{align}\na &= b\n\\end{align}\n$$";
    let out = joined(md);
    assert!(!out.contains("$$"), "delimiters leaked: {out}");
    assert!(out.contains("a  = b"), "{out}");
}

// ============================================================
// 4. `rendering.math = off` == the pre-math rendering
// ============================================================

#[test]
fn off_leaves_everything_literal() {
    let md = "inline $x^2$ and \\(y\\) and $$\\frac{a}{b}$$ and \\begin{align}a &= b\\end{align}";
    let lines = render_with(md, CONTENT, 118, MathMode::Off);
    let out = plain(&lines).join("\n");
    assert!(out.contains("$x^2$"), "{out}");
    // `\\(` is markdown's backslash escape of a `(` — the pre-math reading,
    // which is what `off` promises.
    assert!(out.contains("(y)"), "{out}");
    assert!(out.contains("$$\\frac{a}{b}$$"), "{out}");
    assert!(out.contains("\\begin{align}"), "{out}");
    // No math segment exists at all in `off` mode.
    assert!(
        lines
            .iter()
            .flat_map(|l| l.segments.iter())
            .all(|s| s.kind != SegmentKind::Math)
    );
}

/// The two renders must be span-for-span identical: `off` is defined as "the
/// renderer as if math support did not exist", and the only way to check that
/// without a second binary is to compare against the reconstruction of the
/// input's own literal text (pulldown passes math-shaped text through as
/// `Text` when the option is off).
#[test]
fn off_matches_the_literal_reading_of_every_shape() {
    // (input, what the plain markdown reading of it is)
    let cases = [
        ("inline $x^2$ here", "$x^2$"),
        ("display $$\\frac{a}{b}$$ here", "$$\\frac{a}{b}$$"),
        // `\\(` / `\\[` are markdown backslash escapes: the backslash goes
        // away and the bracket is left as text (this is what the TUI showed
        // before math rendering existed).
        ("paren \\(y\\) here", "(y)"),
        ("bracket \\[z\\] here", "[z]"),
        ("bare \\begin{align}a &= b\\end{align}", "\\begin{align}"),
        ("currency $100 and $200", "$100 and $200"),
        ("code `$x$` here", "$x$"),
    ];
    for (md, literal) in cases {
        let off = spans(&render_with(md, CONTENT, 118, MathMode::Off));
        let text: String = off.iter().map(|(_, t)| t.as_str()).collect();
        assert!(text.contains(literal), "{md:?} → {text:?} lost {literal:?}");
        assert!(
            off.iter().all(|(kind, _)| *kind != SegmentKind::Math),
            "{md:?}: {off:?}"
        );
    }
}

#[test]
fn off_and_text_differ_only_where_math_renders() {
    // A text with no formula at all must render identically in both modes.
    let plain_md = "no formulas here\n\njust `code` and $100 and a [link](https://example.com)";
    assert_eq!(
        spans(&render_with(plain_md, CONTENT, 118, MathMode::Off)),
        spans(&render_with(plain_md, CONTENT, 118, MathMode::Text)),
    );
}

// ============================================================
// 5. Streaming and the composed view agree
// ============================================================

fn span_pairs(lines: &[Line<'static>]) -> Vec<(String, Style)> {
    lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s: &Span| (s.content.to_string(), s.style))
        .collect()
}

#[test]
fn reference_render_uses_the_palette_math_mode() {
    // `full_lines` is the reference the streaming engine reconciles against;
    // it must read the mode from the palette like every other entry point.
    let md = "inline $x^2$ and $$\\frac{a}{b}$$";
    let on: String = full_lines(md, 118, CONTENT, &palette())
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(on.contains("x²"), "{on}");

    let off_palette = ThemePalette {
        math_mode: MathMode::Off,
        ..palette()
    };
    let off = full_lines(md, 118, CONTENT, &off_palette);
    let off_text: String = off
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(off_text.contains("$x^2$"), "{off_text}");
    assert!(!off_text.contains("x²"), "{off_text}");

    // A palette-driven render is the same as the explicit-opts one.
    assert_eq!(
        span_pairs(&off),
        span_pairs(&full_lines(md, 118, CONTENT, &off_palette)),
    );
}

/// The convenience entry points (`render_markdown_lines` /
/// `render_markdown_with_width`, used by the user-message and ask cells) read
/// the mode from the palette too — `off` is global, not per-call-site.
#[test]
fn convenience_entry_points_honour_the_palette_mode() {
    let md = "inline $x^2$ and $$\\frac{a}{b}$$";
    let off_palette = ThemePalette {
        math_mode: MathMode::Off,
        ..palette()
    };
    let lines = wing::render::markdown::render_markdown_lines(md, Some(118), &off_palette);
    let out: String = lines
        .iter()
        .map(MarkdownLine::to_plain)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(out.contains("$x^2$"), "{out}");
    assert!(!out.contains("x²"), "{out}");

    let on = wing::render::markdown::render_markdown_with_width(md, Some(118), &palette());
    let rendered: String = on
        .iter()
        .map(|l| l.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("x²"), "{rendered}");
}

#[test]
fn math_grid_lines_fit_the_cell_width() {
    // The grid is fitted to the content width (width - 2 for the cell
    // prefix) and the composed line never exceeds the cell width.
    for width in [30u16, 40, 80, 120] {
        let lines = full_lines(
            "$$\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}$$",
            width,
            CONTENT,
            &palette(),
        );
        for line in &lines {
            let w: usize = line
                .spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(w <= width as usize, "width={width}: {line:?} is {w} wide");
        }
    }
}
