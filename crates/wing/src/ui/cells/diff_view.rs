//! DiffView — unified diff rendering with green/red coloring.
//!
//! Renders old_text → new_text as a unified diff with:
//!   - Red lines for deletions (prefixed with "-")
//!   - Green lines for additions (prefixed with "+")
//!   - Context lines (no prefix coloring)
//!   - File path header

use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use similar::TextDiff;

use crate::config::ThemePalette;

/// A diff view showing changes to a file.
#[derive(Debug, Clone)]
pub struct DiffView {
    pub path: String,
    pub old_text: Option<String>,
    pub new_text: String,
}

impl DiffView {
    pub fn new(path: String, old_text: Option<String>, new_text: String) -> Self {
        Self {
            path,
            old_text,
            new_text,
        }
    }

    /// Render the diff to lines with context-window collapsing.
    pub fn to_lines(&self, palette: &ThemePalette, context_lines: usize) -> Vec<Line<'static>> {
        let dim = Style::default().fg(palette.dim);
        let mut lines = Vec::new();

        // File path header.
        lines.push(Line::from(Span::styled(format!("  ┌─ {}", self.path), dim)));

        match &self.old_text {
            None => {
                // New file — all additions.
                let add_style = Style::default().fg(palette.success);
                for line in self.new_text.lines() {
                    lines.push(Line::from(Span::styled(format!("  + {line}"), add_style)));
                }
            }
            Some(old) => {
                let diff = TextDiff::from_lines(old, &self.new_text);

                let changes: Vec<_> = diff.iter_all_changes().collect();

                // Mark visible indices: show context_lines around each non-equal change.
                let n = changes.len();
                let mut visible = vec![false; n];
                for (i, change) in changes.iter().enumerate() {
                    if change.tag() != similar::ChangeTag::Equal {
                        let lo = i.saturating_sub(context_lines);
                        let hi = (i + context_lines + 1).min(n);
                        for slot in visible.iter_mut().skip(lo).take(hi - lo) {
                            *slot = true;
                        }
                    }
                }

                // Render visible lines, inserting separators for gaps.
                let mut in_gap = false;
                for (i, change) in changes.iter().enumerate() {
                    if visible[i] {
                        if in_gap {
                            lines.push(Line::from(Span::styled("  ⋮", dim)));
                        }
                        in_gap = false;
                        let (prefix, color) = match change.tag() {
                            similar::ChangeTag::Delete => ("-", palette.danger),
                            similar::ChangeTag::Insert => ("+", palette.success),
                            similar::ChangeTag::Equal => (" ", palette.dim),
                        };
                        let text = change.value().trim_end_matches('\n');
                        lines.push(Line::from(Span::styled(
                            format!("  {prefix} {text}"),
                            Style::default().fg(color),
                        )));
                    } else {
                        in_gap = true;
                    }
                }
            }
        }

        // Footer.
        lines.push(Line::from(Span::styled("  └────", dim)));
        lines.push(Line::from(""));
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemePalette;

    fn p() -> ThemePalette {
        ThemePalette::default()
    }

    #[test]
    fn test_new_file_diff() {
        let diff = DiffView::new("main.rs".into(), None, "fn main() {}".into());
        let lines = diff.to_lines(&p(), 3);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("main.rs"), "missing path: {text}");
        assert!(text.contains("+"), "missing add marker: {text}");
    }

    #[test]
    fn test_modified_file_diff() {
        let old = "fn old() {}\n".into();
        let new = "fn new() {}\n".into();
        let diff = DiffView::new("main.rs".into(), Some(old), new);
        let lines = diff.to_lines(&p(), 3);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("-"), "missing delete marker: {text}");
        assert!(text.contains("+"), "missing add marker: {text}");
    }

    #[test]
    fn test_identical_file_diff() {
        let text = "fn same() {}\n".to_string();
        let diff = DiffView::new("main.rs".into(), Some(text.clone()), text);
        let lines = diff.to_lines(&p(), 3);
        let has_change = lines
            .iter()
            .any(|l| l.to_string().starts_with("  +") || l.to_string().starts_with("  -"));
        assert!(!has_change, "identical file should have no changes");
    }

    #[test]
    fn test_context_collapse_large_file() {
        let old_lines: Vec<String> = (1..=50).map(|i| format!("line {i}")).collect();
        let mut new_lines = old_lines.clone();
        new_lines[24] = "CHANGED".to_string();

        let old = old_lines.join("\n");
        let new = new_lines.join("\n");
        let diff = DiffView::new("big.rs".into(), Some(old), new);
        let lines = diff.to_lines(&p(), 3);

        assert!(
            lines.len() < 20,
            "expected context collapse, got {} lines",
            lines.len()
        );

        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("CHANGED"), "missing changed line");
        assert!(text.contains("⋮"), "missing gap separator");
    }
}
