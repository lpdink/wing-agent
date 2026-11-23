//! Category level of the settings list.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::config::ThemePalette;
use crate::shared::panels::settings::SettingsPanel;
use crate::ui::panel::{cursor_span, label_style};

use super::text::single_row;

pub(super) fn anchor_lines(
    panel: &SettingsPanel,
    palette: &ThemePalette,
    width: usize,
    height: usize,
    description: Option<&str>,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(palette.dim);
    if panel.anchors().is_empty() {
        return vec![Line::from(Span::styled("没有设置分类", dim))];
    }
    let mut lines = Vec::new();
    for index in
        panel.anchors_visible_range(height.saturating_sub(usize::from(description.is_some())))
    {
        let Some(row) = panel.anchor_view(index) else {
            continue;
        };
        let mut spans = vec![cursor_span(row.selected, palette)];
        if row.dirty {
            spans.push(Span::styled("● ", Style::default().fg(palette.success)));
        }
        spans.push(Span::styled(row.title, label_style(row.selected, palette)));
        if row.problems > 0 {
            spans.push(Span::styled(
                format!(" · {} 问题", row.problems),
                Style::default().fg(palette.danger),
            ));
        }
        lines.push(Line::from(spans));
        if row.selected
            && let Some(doc) = description
        {
            lines.push(Line::from(Span::styled(
                format!("  {}", single_row(doc, width.saturating_sub(2))),
                dim,
            )));
        }
    }
    lines
}
