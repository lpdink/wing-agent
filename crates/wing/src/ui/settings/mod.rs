//! Single-column settings list shared by the session TUI and setup.

mod anchors;
mod editor;
mod help;
mod problems;
mod prompt;
mod text;
mod tree;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Clear;

use crate::config::ThemePalette;
use crate::shared::panels::settings::{SettingsPanel, View};

use self::text::{clamp_line, pack_segments, single_row};

pub fn draw(frame: &mut Frame<'_>, panel: &mut SettingsPanel, area: Rect, palette: &ThemePalette) {
    if area.is_empty() {
        return;
    }
    frame.render_widget(Clear, area);
    let width = area.width as usize;
    let height = area.height as usize;
    let dim = Style::default().fg(palette.dim);
    let accent = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);
    let alert = Style::default().fg(palette.warning);
    let prompt = panel.prompt();
    let edit_error = panel
        .edit_state()
        .and_then(|edit| edit.error())
        .map(str::to_owned);
    let search = panel.search_query().is_some();
    let stale = panel.is_stale() && !search && prompt.is_none() && panel.edit_state().is_none();

    let content_min = 1 + usize::from(edit_error.is_some());
    let top = usize::from(height > content_min);
    let notice = usize::from((search || stale) && height >= top + content_min + 2);
    let footer = pack_segments(
        &panel.footer_hint(),
        width,
        height
            .saturating_sub(top + notice + content_min)
            .min(if height >= 5 { 2 } else { 1 }),
    );
    let body = height.saturating_sub(top + notice + footer.len());
    let description = (body >= 3 && prompt.is_none() && panel.edit_state().is_none())
        .then(|| match panel.view() {
            View::Categories => panel.group().map(|group| group.doc.as_str()),
            View::Tree => panel
                .current_row()
                .and_then(|row| panel.row_node(row))
                .map(|node| node.doc.as_str()),
            _ => None,
        })
        .flatten()
        .and_then(|doc| doc.lines().find(|line| !line.trim().is_empty()))
        .map(str::to_owned);
    panel.set_viewport_rows(body.saturating_sub(usize::from(description.is_some())));
    let buf = frame.buffer_mut();
    let mut line_y = area.y;

    if top > 0 {
        let context = match panel.view() {
            View::Categories => Some("分类"),
            View::Tree if !search => panel.group().map(|g| g.title.as_str()),
            View::Problems => Some("待修复"),
            View::Help => Some("帮助"),
            _ => None,
        };
        let mut heading = vec![Span::styled("Settings", accent)];
        if let Some(prompt) = &prompt {
            heading = vec![Span::styled(prompt.title.clone(), accent)];
        } else if let Some(edit) = panel.edit_state() {
            let label = panel
                .current_row()
                .filter(|row| row.root == edit.root() && row.path == edit.path())
                .map_or_else(|| edit.path(), |row| row.label.as_str());
            heading = vec![
                Span::styled("编辑 · ", dim),
                Span::styled(label.to_owned(), accent),
            ];
        } else if search && notice == 0 {
            heading =
                editor::search_line(panel.search_query().unwrap_or_default(), width, palette).spans;
        } else {
            if panel.dirty_count() > 0 {
                heading.push(Span::styled(
                    format!(" · {} unsaved", panel.dirty_count()),
                    alert,
                ));
            }
            if !panel.problems().is_empty() && panel.view() != View::Problems {
                heading.push(Span::styled(
                    format!(" · {} 问题", panel.problems().len()),
                    Style::default().fg(palette.danger),
                ));
            }
            if let Some(label) = context {
                heading.push(Span::styled(format!(" · {label}"), dim));
            }
            if stale && notice == 0 {
                heading.push(Span::styled(" · R 读取磁盘", alert));
            }
        }
        draw_line(buf, area, line_y, Line::from(heading));
        line_y += 1;
    }
    if notice > 0 {
        let line = if let Some(query) = panel.search_query() {
            editor::search_line(query, width, palette)
        } else {
            Line::from(Span::styled("配置已变更 · R 从磁盘读取", alert))
        };
        draw_line(buf, area, line_y, line);
        line_y += 1;
    }

    let lines = if let Some(prompt) = prompt {
        prompt::prompt_lines(&prompt, palette, width, body, top == 0)
    } else if let Some(edit) = panel.edit_state() {
        let mut lines = Vec::new();
        if body > content_min {
            let list_height = body - content_min;
            lines.extend(tree::tree_lines(panel, palette, width, list_height, None));
            lines.resize(list_height, Line::default());
        }
        lines.push(editor::editor_line(edit, width, palette));
        if body > 1
            && let Some(error) = edit_error
        {
            lines.push(Line::from(Span::styled(
                single_row(&error, width),
                Style::default().fg(palette.danger),
            )));
        }
        lines
    } else if search && top == 0 {
        vec![editor::search_line(
            panel.search_query().unwrap_or_default(),
            width,
            palette,
        )]
    } else {
        match panel.view() {
            View::Categories => {
                anchors::anchor_lines(panel, palette, width, body, description.as_deref())
            }
            View::Tree => tree::tree_lines(panel, palette, width, body, description.as_deref()),
            View::Problems => problems::problem_lines(panel, palette, width, body),
            View::Help => {
                let lines = help::help_lines(palette, width, usize::MAX);
                panel.set_help_last_offset(lines.len().saturating_sub(body));
                lines
                    .into_iter()
                    .skip(panel.help_offset())
                    .take(body)
                    .collect()
            }
        }
    };
    for line in lines.into_iter().take(body) {
        draw_line(buf, area, line_y, line);
        line_y += 1;
    }
    for (index, hint) in footer.iter().enumerate() {
        let y = area.bottom() - (footer.len() - index) as u16;
        draw_line(buf, area, y, Line::from(Span::styled(hint.clone(), dim)));
    }
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, line: Line<'static>) {
    buf.set_line(
        area.x,
        y,
        &clamp_line(line, area.width as usize),
        area.width,
    );
}
