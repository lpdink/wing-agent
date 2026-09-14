// Copyright 2025 The wing Authors
// Licensed under the Apache License, Version 2.0.
// Concepts borrowed from codex-rs highlight.rs (Apache 2.0, OpenAI)

//! Syntax highlighting using syntect.
//!
//! Provides real syntax highlighting for code blocks using syntect's
//! easy API (`HighlightLines`). Falls back to plain cyan for unknown languages.
//!
//! Every line handed to syntect goes through [`terminated`]: the syntax set is
//! the "newlines" variant, whose rules are written against the line's
//! terminator. See that function for what happens without one.

use std::borrow::Cow;
use std::sync::OnceLock;

use ratatui::style::Color;
use ratatui::style::Style;
use syntect::easy::HighlightLines;
use syntect::highlighting::FontStyle;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

use super::markdown::types::MarkdownLine;
use super::markdown::types::MarkdownTheme;
use super::markdown::types::SegmentKind;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME_SET: OnceLock<ThemeSet> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

fn theme_set() -> &'static ThemeSet {
    THEME_SET.get_or_init(ThemeSet::load_defaults)
}

/// Detect syntax type from file extension.
pub fn detect_syntax(extension: Option<&str>) -> Option<String> {
    let ss = syntax_set();
    match extension {
        Some(ext) => ss.find_syntax_by_extension(ext).map(|s| s.name.clone()),
        None => None,
    }
}

/// Detect syntax name from a file path (uses the file extension).
pub fn detect_syntax_from_path(path: &str) -> Option<String> {
    let ext = path.rsplit('.').next()?;
    if ext.is_empty() || ext == path {
        return None;
    }
    detect_syntax(Some(ext))
}

/// Highlight a single line independently (no multi-line context).
///
/// Used for streaming rendering where full context is unavailable.
/// Returns styled segments for the line, or None if highlighting fails.
pub fn highlight_single_line(line: &str, lang: &str) -> Option<MarkdownLine> {
    let ss = syntax_set();
    let ts = theme_set();

    let syntax = ss
        .find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_extension(lang))?;

    let theme = &ts.themes["base16-ocean.dark"];
    let mut highlighter = HighlightLines::new(syntax, theme);

    let mut result = MarkdownLine::default();
    for (style, text) in highlight_line_with(&mut highlighter, line)? {
        result.push_segment(SegmentKind::CodeBlock, style, &text);
    }
    Some(result)
}

/// Highlight code and return styled lines, or None if highlighting fails.
///
/// Each returned `MarkdownLine` contains styled segments for that source line.
pub fn highlight_code_lines(
    code: &str,
    lang: Option<&str>,
    _theme: &MarkdownTheme,
) -> Option<Vec<MarkdownLine>> {
    let ss = syntax_set();
    let ts = theme_set();

    let language = lang?;
    let syntax = ss
        .find_syntax_by_token(language)
        .or_else(|| ss.find_syntax_by_extension(language))?;

    let theme = &ts.themes["base16-ocean.dark"];
    let mut highlighter = HighlightLines::new(syntax, theme);

    let mut lines = Vec::new();

    for raw_line in code.lines() {
        let ops = highlight_line_with(&mut highlighter, raw_line)?;

        let mut line = MarkdownLine::default();
        for (style, text) in ops {
            line.push_segment(SegmentKind::CodeBlock, style, &text);
        }
        lines.push(line);
    }

    Some(lines)
}

/// Build a stateful highlighter for incremental (line-at-a-time) code
/// highlighting — used by the streaming renderer so already-highlighted
/// lines are never recomputed.
///
/// Returns None when the language is unknown (callers fall back to plain).
pub fn new_highlighter(lang: &str) -> Option<HighlightLines<'static>> {
    let ss = syntax_set();
    let syntax = ss
        .find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_extension(lang))?;
    let theme = &theme_set().themes["base16-ocean.dark"];
    Some(HighlightLines::new(syntax, theme))
}

/// Build a stateful highlighter for a file, resolving the language from the
/// file name (extension or, for extension-less files, the name itself —
/// `Makefile`, `Dockerfile`, `.gitignore`) and falling back to the first
/// line (shebangs) when the name does not resolve.
///
/// `first_line` is the file's first line when the caller has the content at
/// hand; `None` skips that fallback. Returns None when nothing matches.
pub fn new_highlighter_for_file(
    path: &str,
    first_line: Option<&str>,
) -> Option<HighlightLines<'static>> {
    let ss = syntax_set();
    let basename = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let syntax = ss
        // Extension, then syntax name (case-insensitive) — this is what
        // resolves `Makefile` / `Dockerfile` / `.gitignore`.
        .find_syntax_by_token(basename)
        .or_else(|| {
            let ext = basename.rsplit_once('.')?.1;
            if ext.is_empty() {
                None
            } else {
                ss.find_syntax_by_extension(ext)
            }
        })
        .or_else(|| first_line.and_then(|line| ss.find_syntax_by_first_line(line)))?;
    let theme = &theme_set().themes["base16-ocean.dark"];
    Some(HighlightLines::new(syntax, theme))
}

/// `line` with the terminator the syntax definitions expect.
///
/// The syntax set is the "newlines" variant (`two_face::syntax::extra_newlines`,
/// the mode syntect documents as the robust one), so its rules are written with
/// the terminator in hand — Python's line comment pops on `$\n`, for instance.
/// A line handed over *without* it leaves the parser inside that construct for
/// good: the first trailing `# comment` of a file then paints every following
/// line with comment colors (a diff of a `.py` file losing its highlighting
/// from its first inline comment onward).
///
/// Callers that return syntect's borrowed spans have to strip the appended
/// terminator again — see [`highlight_line_with`].
fn terminated(line: &str) -> Cow<'_, str> {
    if line.ends_with('\n') {
        return Cow::Borrowed(line);
    }
    let mut terminated = String::with_capacity(line.len() + 1);
    terminated.push_str(line);
    terminated.push('\n');
    Cow::Owned(terminated)
}

/// Drop the terminator [`terminated`] appended, from the spans parsed with it.
///
/// The spans partition the parsed line, so the appended `\n` is always the tail
/// of the last one — a span of its own when the line ends inside a scope.
fn strip_terminator(spans: &mut Vec<(Style, String)>) {
    if let Some((_, text)) = spans.last_mut()
        && text.ends_with('\n')
    {
        text.pop();
        if text.is_empty() {
            spans.pop();
        }
    }
}

/// Advance a highlighter's parse/highlight state over `line` WITHOUT building
/// styled output.
///
/// `HighlightLines::highlight_line` returns borrowed slices, so skipping the
/// `(Style, String)` conversion saves one allocation per syntect op — this is
/// the cheap path for lines whose rendering is not needed (collapsed context,
/// or the old revision's side of an unchanged line). A line that arrives
/// without its terminator is still copied once, to feed it to the parser.
pub fn advance_line(highlighter: &mut HighlightLines<'static>, line: &str) {
    // Parse errors leave the state where it was, matching `highlight_line_with`.
    let _ = highlighter.highlight_line(&terminated(line), syntax_set());
}

/// Highlight a single line, ADVANCING the highlighter state.
///
/// The state is positioned "after the previous line" — highlighting line N
/// then N+1 with the same highlighter reproduces `highlight_code_lines`
/// exactly. The returned spans carry the caller's line verbatim: the
/// terminator [`terminated`] appends for the parser is trimmed back off.
pub fn highlight_line_with(
    highlighter: &mut HighlightLines<'static>,
    line: &str,
) -> Option<Vec<(Style, String)>> {
    let line = terminated(line);
    let ops = highlighter.highlight_line(&line, syntax_set()).ok()?;
    let mut spans: Vec<(Style, String)> = ops
        .into_iter()
        .map(|(style, text)| (convert_syntect_style(style), text.to_string()))
        .collect();
    if matches!(line, Cow::Owned(_)) {
        strip_terminator(&mut spans);
    }
    Some(spans)
}

fn convert_syntect_style(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let color = Color::Rgb(fg.r, fg.g, fg.b);

    let mut ratatui_style = Style::new().fg(color);

    if style.font_style.contains(FontStyle::BOLD) {
        ratatui_style = ratatui_style.bold();
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        ratatui_style = ratatui_style.italic();
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        ratatui_style = ratatui_style.underlined();
    }

    ratatui_style
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_syntax() {
        assert_eq!(detect_syntax(Some("rs")), Some("Rust".to_string()));
        assert_eq!(detect_syntax(Some("py")), Some("Python".to_string()));
        assert_eq!(detect_syntax(Some("xyz_unknown")), None);
        assert_eq!(detect_syntax(None), None);
    }

    #[test]
    fn test_highlight_code_lines_rust() {
        let theme = MarkdownTheme::default();
        let result = highlight_code_lines("fn main() {}", Some("rust"), &theme);
        assert!(result.is_some());
        let lines = result.unwrap();
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].segments.is_empty());
    }

    /// Extension-less files resolve by file name, then by the first line.
    #[test]
    fn test_highlighter_for_file() {
        assert!(new_highlighter_for_file("Makefile", None).is_some());
        assert!(new_highlighter_for_file("docker/Dockerfile", None).is_some());
        assert!(new_highlighter_for_file("src/main.rs", None).is_some());
        assert!(
            new_highlighter_for_file("scripts/zzz-no-language", Some("#!/usr/bin/env python3"))
                .is_some(),
            "shebang should resolve the language"
        );
        assert!(new_highlighter_for_file("scripts/zzz-no-language", None).is_none());
        assert!(new_highlighter_for_file("data.zzzq", None).is_none());
    }

    #[test]
    fn test_highlight_code_lines_unknown() {
        let theme = MarkdownTheme::default();
        let result = highlight_code_lines("some text", Some("zzz_nonexistent"), &theme);
        assert!(result.is_none());
    }

    #[test]
    fn test_highlight_code_lines_none_lang() {
        let theme = MarkdownTheme::default();
        let result = highlight_code_lines("some text", None, &theme);
        assert!(result.is_none());
    }

    /// Every caller feeds lines without their terminator (that is what
    /// `str::lines()` and the diff payloads hand over). The syntax definitions
    /// are the "newlines" variant, so a line comment parses `# iflag` against
    /// the terminating `$\n`: without one the parser stayed inside the comment
    /// context for good and every later line came back comment-colored.
    #[test]
    fn test_trailing_line_comment_does_not_leak() {
        let lines = ["a = 0  # iflag", "b = 1", "c = 2  # oflag", "d = 3"];
        let mut hl = new_highlighter("python").expect("python syntax");

        for line in lines {
            let stateful = highlight_line_with(&mut hl, line).expect("highlight");
            // These are independent statements, so a fresh highlighter is the
            // ground truth for each of them.
            let fresh =
                highlight_line_with(&mut new_highlighter("python").expect("python syntax"), line)
                    .expect("highlight");
            assert_eq!(stateful, fresh, "line {line:?} was parsed out of context");
        }
    }

    /// The whole-block path (fenced code blocks) is fed by the same helper and
    /// must render what per-line highlighting produces for the same lines.
    #[test]
    fn test_code_block_comment_does_not_leak() {
        let theme = MarkdownTheme::default();
        let code = "a = 0  # iflag\nb = 1\n";
        let block = highlight_code_lines(code, Some("py"), &theme).expect("highlight");

        // These are independent statements, so a fresh highlighter per line is
        // the ground truth.
        let fresh: Vec<MarkdownLine> = ["a = 0  # iflag", "b = 1"]
            .iter()
            .map(|line| {
                let mut hl = new_highlighter("python").expect("python syntax");
                let mut md = MarkdownLine::default();
                for (style, text) in highlight_line_with(&mut hl, line).expect("highlight") {
                    md.push_segment(SegmentKind::CodeBlock, style, &text);
                }
                md
            })
            .collect();

        let shape = |lines: &[MarkdownLine]| -> Vec<Vec<(String, String)>> {
            lines
                .iter()
                .map(|line| {
                    line.segments
                        .iter()
                        .map(|segment| (segment.text.clone(), format!("{:?}", segment.style.fg)))
                        .collect()
                })
                .collect()
        };
        assert_eq!(
            shape(&block),
            shape(&fresh),
            "the comment leaked into the rest of the block"
        );
    }

    /// The terminator exists for the parser only — renderers get the caller's
    /// bytes back, terminator or not.
    #[test]
    fn test_spans_reproduce_the_line_verbatim() {
        let mut hl = new_highlighter("rust").expect("rust syntax");
        for line in ["let x = 1;", "// note", "    ", "", "let y = 2;\n"] {
            let spans = highlight_line_with(&mut hl, line).expect("highlight");
            let text: String = spans.iter().map(|(_, text)| text.as_str()).collect();
            assert_eq!(text, line, "spans must reproduce {line:?}");
        }
    }
}
