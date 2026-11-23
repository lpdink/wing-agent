//! Confirmation and variant selection in the settings list.

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::config::ThemePalette;
use crate::shared::panels::settings::PromptView;

use super::text::LineBuf;
use super::text::wrap_rows;
use crate::shared::panels::window_range;

pub(super) fn prompt_lines(
    prompt: &PromptView,
    palette: &ThemePalette,
    width: usize,
    height: usize,
    include_title: bool,
) -> Vec<Line<'static>> {
    let mut buf = LineBuf::new(width, height);
    let accent = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);
    let text = Style::default().fg(palette.text);
    if include_title && (prompt.options.is_empty() || height > 1) {
        buf.text(&prompt.title, accent);
    }
    let option_rows = prompt.options.len().min(buf.remaining());
    let detail_rows = buf.remaining() - option_rows;
    for line in prompt
        .lines
        .iter()
        .flat_map(|line| wrap_rows(line, width, detail_rows))
        .take(detail_rows)
    {
        buf.text(&line, text);
    }
    for index in window_range(prompt.cursor, prompt.options.len(), option_rows) {
        let selected = index == prompt.cursor;
        buf.push(Line::from(vec![
            crate::ui::panel::cursor_span(selected, palette),
            Span::styled(
                prompt.options[index].clone(),
                if selected { accent } else { text },
            ),
        ]));
    }
    buf.take()
}
