//! Drift detection: the normalization scanner's “which regions are not prose”
//! model must agree with what the parser actually does.
//!
//! `math::normalize_delimiters` rewrites `\(…\)` / `\[…\]` / bare environments
//! into `$…$` / `$$…$$`, but it must never touch a region markdown does not
//! parse as prose: code spans and fences, HTML blocks, link reference
//! definitions, link/image destinations. Those regions are exactly the ones the
//! renderer marks as `InlineCode` / `CodeBlock` / `Link` — so the check is:
//!
//! * **no region rewritten** (this file): the code and link segments of the
//!   render must be byte-identical with `rendering.math = text` and `= off`;
//!   any difference means a rewrite landed inside one of them — the failure
//!   mode of three separate defects;
//! * **prose is still rewritten** (the shape tests in `math_render.rs` and the
//!   `stream_render_reconcile` matrix): a delimiter pair outside those regions
//!   must be rewritten, otherwise the scanner over-protects.
//!
//! The corpus is generated here (no fixtures on disk) with a fixed seed, so the
//! test is deterministic and reproducible.
//!
//! Mutation check (each was run by hand, see the r4 report): reverting any of
//! the three heuristics — `html_block_start` back to “any `<…`”, the reference
//! definition continuation to “any indented line”, or `FenceTrack::step`'s
//! list-item handling — makes this test fail.

use wing::config::ThemePalette;
use wing::config::rendering::MathMode;
use wing::render::markdown::Profile;
use wing::render::markdown::types::{MarkdownLine, SegmentKind};
use wing::render::markdown::{RenderOpts, render_markdown_lines_with};

/// A tiny deterministic PRNG (xorshift64*) — no dev-dependency needed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next() % items.len() as u64) as usize]
    }
}

/// Markdown pieces that carry a LaTeX delimiter or surround one: fences (with
/// and without block prefixes), HTML in all shapes CommonMark distinguishes,
/// reference definitions, lists (nested, empty, with tabs), quotes, inline code
/// and links — plus prose.
const PIECES: &[&str] = &[
    // delimiters (the thing that must be rewritten exactly once, if prose)
    "\\(x\\)",
    "\\[z\\]",
    "\\begin{align}a\\end{align}",
    "$q$",
    "$$w$$",
    "\\ce{2H2O}",
    "text \\(a\\) more",
    "中文 \\(b\\) 尾巴",
    // fences
    "~~~",
    "```",
    "~~~~",
    "  ~~~",
    "   ```",
    "> ~~~",
    "- ~~~",
    "  - ```",
    "> - ~~~",
    "1. ~~~",
    // containers
    "- a",
    "- ",
    "1. b",
    "  - c",
    "> quote",
    ">",
    "-\ttab",
    // HTML blocks / inline HTML
    "<div>",
    "</div>",
    "<pre>",
    "<b>bold</b>",
    "<3",
    "<3 x",
    "<!-- c -->",
    "<?php y ?>",
    "<!DOCTYPE html>",
    "<![CDATA[ x ]]>",
    "<b>",
    "</b>",
    "<span a=\"x>y\">",
    // reference definitions
    "[ref]: http://x",
    "[a b]: /u",
    "[r]:",
    "\"title\"",
    "(title)",
    // links / images / inline code / tables
    "[a](http://x/\\(y\\))",
    "![i](/u)",
    "[r]",
    "`$PATH`",
    "`\\(y\\)`",
    "| a | b |",
    // structure
    "",
    " ",
    "    ",
    "\t",
    "para",
    "中文段落",
    "# head",
    "---",
];

/// Deterministic documents that place every protected-region opener next to
/// every container/interrupter shape — the adjacencies the defects lived in
/// (a broad HTML rule or a lax reference-definition continuation
/// only shows up when the *next* line is a fence opener, and a list-item fence
/// only when a marker/blank follows).
fn structured_corpus() -> Vec<String> {
    // `  - \t` / `-\t` put a tab after the marker (column vs byte padding).
    const PREFIXES: &[&str] = &["", "> ", "- ", "1. ", "  - ", "\t", "  - \t", "-\t"];
    const OPENERS: &[&str] = &[
        "~~~",
        "```",
        "<3",
        "<b>bold</b>",
        "<!-- c -->",
        "<?php y ?>",
        "<b>",
        "<div>",
        "[ref]: http://x",
        "[ref]:",
        "[a b]: /u",
        "   ~~~",
    ];
    const FOLLOWERS: &[&str] = &[
        "", "- a", "- ", "1. b", "  - c", "> q", "para", "\t", "  ", "      x",
    ];
    // Blanks between the follower and the closer (or the closer and the tail):
    // the empty-item and definition-continuation rules only differ with blanks
    // in play.
    const GAPS: &[&str] = &["", "\n", "\n  \n\n"];
    const CLOSERS: &[&str] = &["", "  ~~~", "~~~", "> ~~~", "    ~~~~", "   ~~~"];
    const TAILS: &[&str] = &["\\(x\\) after", "$$z$$"];
    let mut docs = Vec::new();
    for prefix in PREFIXES {
        for opener in OPENERS {
            for follower in FOLLOWERS {
                for inner in GAPS {
                    for closer in CLOSERS {
                        for tail in TAILS {
                            let mut doc = String::new();
                            doc.push_str(prefix);
                            doc.push_str(opener);
                            doc.push('\n');
                            if !follower.is_empty() {
                                doc.push_str(follower);
                                doc.push('\n');
                            }
                            doc.push_str(inner);
                            if !closer.is_empty() {
                                doc.push_str(closer);
                                doc.push('\n');
                            }
                            doc.push('\n');
                            doc.push_str(tail);
                            doc.push('\n');
                            docs.push(doc);
                        }
                    }
                }
            }
        }
    }
    docs
}

/// Random documents built from [`PIECES`].
fn corpus(count: usize) -> Vec<String> {
    let mut rng = Rng(0x5eed_1234_5678_9abc);
    let mut docs = Vec::with_capacity(count);
    for _ in 0..count {
        let mut doc = String::new();
        for _ in 0..rng.next() % 9 + 3 {
            doc.push_str(rng.pick(PIECES));
            if rng.next().is_multiple_of(3) {
                doc.push_str("\n\n");
            } else {
                doc.push('\n');
            }
        }
        docs.push(doc);
    }
    docs.extend(structured_corpus());
    docs
}

/// The text of every region the parser does **not** treat as prose.
fn protected_text(lines: &[MarkdownLine]) -> Vec<(SegmentKind, String)> {
    lines
        .iter()
        .flat_map(|line| line.segments.iter())
        .filter(|segment| {
            matches!(
                segment.kind,
                SegmentKind::CodeBlock | SegmentKind::InlineCode | SegmentKind::Link
            )
        })
        .map(|segment| (segment.kind, segment.text.clone()))
        .collect()
}

fn render(doc: &str, profile: Profile, math: MathMode) -> Vec<MarkdownLine> {
    let palette = ThemePalette::default();
    let opts = RenderOpts {
        profile,
        math,
        ..RenderOpts::default()
    };
    render_markdown_lines_with(doc, Some(80), &palette, opts)
}

#[test]
fn normalization_never_rewrites_a_non_prose_region() {
    let docs = corpus(300);
    assert!(docs.len() > 1000, "corpus too small: {}", docs.len());
    let mut hits = 0;
    let mut first_failure = String::new();
    for doc in &docs {
        for profile in [Profile::Content, Profile::Thinking] {
            let text = protected_text(&render(doc, profile, MathMode::Text));
            let off = protected_text(&render(doc, profile, MathMode::Off));
            if text != off {
                hits += 1;
                if first_failure.is_empty() {
                    first_failure = format!(
                        "profile={profile:?}\ndoc={doc:?}\nwith math={text:?}\nwith off={off:?}"
                    );
                }
            }
        }
    }
    assert_eq!(
        hits, 0,
        "normalization rewrote a code/link region in {hits} render(s):\n{first_failure}"
    );
}

/// The mirror image: a delimiter pair that lives in plain prose **must** be
/// rewritten (`\\(x\\)` → `$x$`/rendered), otherwise the scanner over-protects
/// (the r4/S1 failure mode: a list-item fence reading swallowed the prose that
/// followed it).
#[test]
fn prose_delimiters_are_always_rewritten() {
    let mut over_protected = Vec::new();
    for doc in [
        // Bare prose.
        "\\(x\\) after\n",
        // After an item fence that really closed.
        "- ~~~\n  a\n  ~~~\n\n\\(x\\) after\n",
        // After a sibling item's (empty) fence.
        "- ~~~\n- a\n  ~~~\n\n\\(x\\) after\n",
        "- ~~~\n- \n  ~~~\n\n\\(x\\) after\n",
        // After a quote-in-item fence.
        "- > ~~~\n- a\n  ~~~\n\n\\(x\\) after\n",
        "- > ~~~\n  - c\n  ~~~\n\n\\(x\\) after\n",
        // After a closed HTML block / reference definition.
        "<div>\n\\(y\\)\n</div>\n\n\\(x\\) after\n",
        "<!-- c -->\n\n\\(x\\) after\n",
        "[ref]: http://x\n  \"title\"\n\n\\(x\\) after\n",
        // After an indented code block that ended.
        "    code\n\n\\(x\\) after\n",
    ] {
        for profile in [Profile::Content, Profile::Thinking] {
            let text = render(doc, profile, MathMode::Text);
            let rendered: String = text
                .iter()
                .flat_map(|l| l.segments.iter())
                .map(|s| s.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            if !rendered.contains("x ") && !rendered.contains("x\u{3000}") {
                over_protected.push(format!("profile={profile:?} doc={doc:?} => {rendered:?}"));
            }
        }
    }
    assert!(
        over_protected.is_empty(),
        "prose delimiters were left unrewritten:\n{}",
        over_protected.join("\n")
    );
}

/// Regions whose content the parser emits as plain `Text` — a whole-line HTML
/// block — are invisible to the segment criterion above, so they get a direct
/// one: for a document that is **entirely** non-prose, the rendered text must
/// be the same with math on and off (no rewrite anywhere).
///
/// The HTML variant:
/// `<b>\n~~~~\n- a\n\\(x\\)\n` is one HTML block, so `\\(x\\)` must stay
/// verbatim.
#[test]
fn monolithic_non_prose_documents_are_never_rewritten() {
    const DOCS: &[&str] = &[
        // HTML block (type 7) swallowing a fence line and a list marker.
        "<b>\n~~~~\n- a\n\\(x\\)\n",
        "<b>\n```\n> q\n\\(x\\)\n",
        // Type 6 / type 1 blocks.
        "<div>\n~~~\n- a\n\\(x\\)\n",
        "<pre>\n~~~~\n1. b\n\\(x\\)\n",
        // A code block that never closes, an indented code block, a reference
        // definition.
        "```\n\\(x\\)\n",
        "    \\(x\\)\n",
        "[ref]: http://x/\\(y\\)\n",
        // …including inside a container.
        "> <b>\n> ~~~\n> - a\n> \\(x\\)\n",
    ];
    let text = |doc: &str, math: MathMode| {
        render(doc, Profile::Content, math)
            .iter()
            .flat_map(|l| l.segments.iter())
            .map(|s| s.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    };
    for doc in DOCS {
        assert_eq!(
            text(doc, MathMode::Text),
            text(doc, MathMode::Off),
            "{doc:?}: a non-prose document was rewritten"
        );
    }
    // …while the same delimiters in prose are rewritten (the assertion above
    // must not be vacuously true).
    assert_ne!(
        text("\\(x\\)\n", MathMode::Text),
        text("\\(x\\)\n", MathMode::Off)
    );
    assert_ne!(
        text("<b>\n~~~~\n- a\n\\(x\\)\n\n\\(y\\)\n", MathMode::Text),
        text("<b>\n~~~~\n- a\n\\(x\\)\n\n\\(y\\)\n", MathMode::Off),
        "the prose after the block ends must still be rewritten"
    );
}

/// Sanity: the corpus really does exercise the protected regions (otherwise
/// the test above would be vacuous).
#[test]
fn corpus_exercises_every_protected_region() {
    let docs = corpus(300);
    let mut code = 0;
    let mut link = 0;
    let mut fence = 0;
    for doc in &docs {
        let lines = render(doc, Profile::Content, MathMode::Off);
        for segment in lines.iter().flat_map(|l| l.segments.iter()) {
            match segment.kind {
                SegmentKind::InlineCode => code += 1,
                SegmentKind::Link => link += 1,
                _ => {}
            }
        }
        fence += lines
            .iter()
            .flat_map(|l| l.segments.iter())
            .filter(|s| s.kind == SegmentKind::CodeBlock && !s.text.trim().is_empty())
            .count();
    }
    assert!(code >= 20, "corpus has too few inline-code regions: {code}");
    assert!(link >= 20, "corpus has too few link regions: {link}");
    assert!(fence >= 20, "corpus has too few fenced-code lines: {fence}");
}

/// The palette-driven entry point (what the TUI uses) must agree with the
/// low-level one, so the drift test covers the path that actually ships.
#[test]
fn palette_entry_point_uses_the_same_normalization() {
    let doc = "\\(x\\) and\n\n~~~\n\\(y\\)\n~~~\n";
    let palette = ThemePalette::default();
    let via_palette = wing::render::markdown::render_markdown_lines(doc, Some(80), &palette);
    let via_opts = render(doc, Profile::Content, palette.math_mode);
    let text = |lines: &[MarkdownLine]| {
        lines
            .iter()
            .flat_map(|l| l.segments.iter())
            .map(|s| s.text.clone())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(text(&via_palette), text(&via_opts));
    // …and the fence content stays verbatim in both.
    assert!(text(&via_palette).contains("\\(y\\)"));
}
