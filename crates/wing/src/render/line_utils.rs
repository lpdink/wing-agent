// Copyright 2025 OpenAI, Inc.
// Licensed under the Apache License, Version 2.0.
// Adapted from codex-rs (https://github.com/openai/codex)

//! Utility functions for ratatui `Line` manipulation.

use ratatui::text::Line;
use ratatui::text::Span;

/// Clone a borrowed ratatui `Line` into an owned `'static` line.
pub fn line_to_static(line: &Line<'_>) -> Line<'static> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .iter()
            .map(|s| Span {
                style: s.style,
                content: std::borrow::Cow::Owned(s.content.to_string()),
            })
            .collect(),
    }
}

/// Append owned copies of borrowed lines to `out`.
pub fn push_owned_lines<'a>(src: &[Line<'a>], out: &mut Vec<Line<'static>>) {
    for l in src {
        out.push(line_to_static(l));
    }
}

/// Prefix each line with `initial_prefix` for the first line and
/// `subsequent_prefix` for following lines. Returns a new Vec of owned lines.
pub fn prefix_lines(
    lines: Vec<Line<'static>>,
    initial_prefix: Span<'static>,
    subsequent_prefix: Span<'static>,
) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .enumerate()
        .map(|(i, l)| {
            let mut spans = Vec::with_capacity(l.spans.len() + 1);
            spans.push(if i == 0 {
                initial_prefix.clone()
            } else {
                subsequent_prefix.clone()
            });
            spans.extend(l.spans);
            Line::from(spans).style(l.style)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_line_to_static() {
        let borrowed = Line::from("hello");
        let owned = line_to_static(&borrowed);
        assert_eq!(owned.to_string(), "hello");
    }

    #[test]
    fn test_prefix_lines() {
        let lines = vec![Line::from("a"), Line::from("b"), Line::from("c")];
        let prefixed = prefix_lines(lines, Span::from("> "), Span::from("  "));
        assert_eq!(prefixed[0].to_string(), "> a");
        assert_eq!(prefixed[1].to_string(), "  b");
        assert_eq!(prefixed[2].to_string(), "  c");
    }
}
