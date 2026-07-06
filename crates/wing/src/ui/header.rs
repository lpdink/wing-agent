//! Chat header — wing "W" logo + What's new box, side-by-side.
//!
//! Left-aligned, Unicode block characters (█) for the logo,
//! box-drawing for the release notes panel.
//! Scrolls up naturally as messages arrive.

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::config::ThemePalette;

/// "W" rendered with Unicode full-block characters.
/// Each entry is one row; all ██ chars share the accent color.
const WING_ART: &[&str] = &[
    "██              ██",
    " ██            ██",
    "  ██  ██████  ██",
    "   ██ ██  ██ ██",
    "    ████  ████",
];

/// Padded column width for the art (art width + spacing before box).
const ART_COL: usize = 24;

/// Release notes — update per version.
const RELEASE_NOTES: &[&str] = &[
    "Async proactive context compaction in background",
    "/reload hot-reload config, hooks, and commands",
    "Theme slots for thinking and tool_result colors",
    "Input area word-wrap for long text",
];

/// Build header lines for the chat view.
pub fn build_header_lines(palette: &ThemePalette) -> Vec<Line<'static>> {
    let accent = Style::default().fg(palette.accent);
    let dim = Style::default().fg(palette.dim);
    let text_style = Style::default().fg(palette.text);
    let bold_accent = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);

    let version = env!("CARGO_PKG_VERSION");
    let commit = env!("WING_COMMIT_HASH");

    // ── Box (right panel) ──
    let title = format!(" wing v{version} ({commit}) ");
    let max_content = RELEASE_NOTES
        .iter()
        .map(|n| n.len() + 2) // +2 for "- " prefix
        .max()
        .unwrap_or(0);
    let box_inner = (max_content + 2).max(title.chars().count()); // +2 trailing pad
    let box_total = box_inner + 4; // borders: │ + space + inner + space + │

    let box_lines = build_box_lines(
        &title,
        box_inner,
        box_total,
        accent,
        bold_accent,
        dim,
        text_style,
    );

    // ── Side-by-side layout ──
    let total_rows = WING_ART.len().max(box_lines.len());
    let mut result: Vec<Line<'static>> = Vec::with_capacity(total_rows + 2);

    // Top padding — visual separation from status bar.
    result.push(Line::from(""));

    for i in 0..total_rows {
        let mut spans: Vec<Span<'static>> = Vec::new();

        // Left: W art row (padded to ART_COL).
        if i < WING_ART.len() {
            let art = WING_ART[i];
            let art_w = art.chars().count();
            let pad = ART_COL.saturating_sub(art_w);
            spans.push(Span::styled(art.to_string(), accent));
            spans.push(Span::from(" ".repeat(pad)));
        } else {
            spans.push(Span::from(" ".repeat(ART_COL)));
        }

        // Right: box row.
        if i < box_lines.len() {
            spans.extend(
                box_lines[i]
                    .spans
                    .iter()
                    .map(|s| Span::styled(s.content.to_string(), s.style)),
            );
        }

        result.push(Line::from(spans));
    }

    // Trailing blank line.
    result.push(Line::from(""));
    result
}

/// Build the "What's new" box as a Vec<Line>.
fn build_box_lines(
    title: &str,
    inner: usize,
    total: usize,
    _accent: Style,
    bold_accent: Style,
    dim: Style,
    text_style: Style,
) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(RELEASE_NOTES.len() + 2);

    // Top border: ╭─ title ───╮
    let dash_after = total.saturating_sub(2 + title.chars().count() + 1);
    lines.push(Line::from(vec![
        Span::styled("╭─", dim),
        Span::styled(title.to_string(), bold_accent),
        Span::styled(format!("{}╮", "─".repeat(dash_after)), dim),
    ]));

    // Content: │ - note  │
    for &note in RELEASE_NOTES {
        let content = format!("- {note}");
        let pad = inner.saturating_sub(content.chars().count());
        lines.push(Line::from(vec![
            Span::styled("│ ", dim),
            Span::styled(format!("{content}{}", " ".repeat(pad)), text_style),
            Span::styled(" │", dim),
        ]));
    }

    // Bottom border: ╰───╯
    lines.push(Line::from(vec![Span::styled(
        format!("╰{}╯", "─".repeat(total - 2)),
        dim,
    )]));

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lines_not_empty() {
        let palette = ThemePalette::default();
        let lines = build_header_lines(&palette);
        // max(art, box) + 1 trailing blank
        let min_rows = WING_ART.len().max(RELEASE_NOTES.len() + 2) + 1;
        assert!(lines.len() >= min_rows);
    }

    #[test]
    fn wing_art_has_five_rows() {
        assert_eq!(WING_ART.len(), 5);
    }

    #[test]
    fn art_rows_fit_in_art_col() {
        for row in WING_ART {
            assert!(
                row.chars().count() <= ART_COL,
                "art row too wide: {} chars (max {ART_COL}): {row}",
                row.chars().count()
            );
        }
    }

    #[test]
    fn box_borders_consistent_width() {
        let palette = ThemePalette::default();
        let lines = build_header_lines(&palette);

        let top = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.starts_with("╭─")))
            .expect("top border missing");

        let bot = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.starts_with("╰")))
            .expect("bottom border missing");

        // Box border widths (counting only box spans, not art padding).
        fn box_char_count(line: &Line<'_>) -> usize {
            line.spans
                .iter()
                .filter(|s| {
                    s.content.starts_with("╭")
                        || s.content.starts_with("╰")
                        || s.content.starts_with("─")
                        || s.content.starts_with("╮")
                        || s.content.starts_with("╯")
                        || s.content.contains("What")
                        || s.content.contains("wing")
                })
                .map(|s| s.content.chars().count())
                .sum()
        }

        let top_w = box_char_count(top);
        let bot_w = box_char_count(bot);
        assert_eq!(
            top_w, bot_w,
            "top and bottom borders differ: {top_w} vs {bot_w}"
        );
    }

    #[test]
    fn dynamic_box_width_fits_content() {
        let palette = ThemePalette::default();
        let lines = build_header_lines(&palette);

        // Find a content line (starts with │ after art padding).
        let content_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("- Async")))
            .expect("release note line missing");

        // The line should contain the full note text.
        let full: String = content_line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            full.contains("Async proactive context compaction in background"),
            "note text truncated: {full}"
        );
    }
}
