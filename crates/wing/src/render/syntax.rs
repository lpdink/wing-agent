// Copyright 2025 The wing Authors
// Licensed under the Apache License, Version 2.0.
// Concepts borrowed from codex-rs highlight.rs (Apache 2.0, OpenAI)

//! Syntax highlighting using syntect.
//!
//! Provides real syntax highlighting for code blocks using syntect's
//! easy API (`HighlightLines`). Falls back to plain cyan for unknown languages.

use std::sync::OnceLock;

use ratatui::style::Color;
use ratatui::style::Style;
use syntect::easy::HighlightLines;
use syntect::highlighting::FontStyle;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;

use super::markdown::types::MarkdownLine;
use super::markdown::types::MarkdownTheme;

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

    let ops = highlighter.highlight_line(line, ss).ok()?;
    let mut result = MarkdownLine::default();
    for (style, text) in ops {
        let ratatui_style = convert_syntect_style(style);
        result.push_segment(ratatui_style, text);
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
        let ops = match highlighter.highlight_line(raw_line, ss) {
            Ok(ops) => ops,
            Err(_) => return None,
        };

        let mut line = MarkdownLine::default();
        for (style, text) in ops {
            let ratatui_style = convert_syntect_style(style);
            line.push_segment(ratatui_style, text);
        }
        lines.push(line);
    }

    Some(lines)
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
}
