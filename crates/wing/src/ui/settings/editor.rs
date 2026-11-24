//! Full-width, cursor-relative viewport over the editor's public masked buffer.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::config::ThemePalette;
use crate::shared::panels::settings::EditState;

use super::text::display_width;

pub(super) fn editor_line(
    state: &EditState,
    width: usize,
    palette: &ThemePalette,
) -> Line<'static> {
    let mut line = input_line(&state.visible_buffer(), state.cursor(), width, palette);
    if state.is_secret() && state.buffer_len() == 0 {
        line.spans.push(Span::styled(
            " 输入新密钥",
            Style::default().fg(palette.dim),
        ));
    }
    line.style = Style::default().bg(palette.surface);
    line
}

pub(super) fn search_line(query: &str, width: usize, palette: &ThemePalette) -> Line<'static> {
    let prefix = if width >= 4 { "/ " } else { "" };
    let mut line = input_line(query, query.chars().count(), width - prefix.len(), palette);
    line.spans
        .insert(0, Span::styled(prefix, Style::default().fg(palette.accent)));
    line
}

fn input_line(value: &str, cursor: usize, width: usize, palette: &ThemePalette) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let display: String = value
        .chars()
        .map(|ch| match ch {
            '\n' | '\r' => '↵',
            '\t' => '⇥',
            ch if ch.is_control() => '�',
            ch => ch,
        })
        .collect();
    let content = Span::raw(display);
    let glyphs: Vec<_> = content
        .styled_graphemes(Style::default())
        .map(|glyph| (glyph.symbol, display_width(glyph.symbol)))
        .chain([(" ", 1)]) // cursor at end of the buffer
        .collect();
    let mut chars = 0;
    let cursor = glyphs
        .iter()
        .position(|(glyph, _)| {
            chars += glyph.chars().count();
            chars > cursor
        })
        .unwrap_or(glyphs.len() - 1);
    let cursor_width = glyphs[cursor].1.clamp(1, width);
    let mut start = cursor;
    let mut used = cursor_width;
    while start > 0 && used + glyphs[start - 1].1 <= width {
        start -= 1;
        used += glyphs[start].1;
    }
    let text = Style::default().fg(palette.text);
    let mut remaining = width;
    let mut spans = Vec::new();
    for (index, &(glyph, glyph_width)) in glyphs.iter().enumerate().skip(start) {
        if index == cursor {
            let glyph = if glyph_width == cursor_width {
                glyph
            } else {
                " "
            };
            spans.push(Span::styled(
                glyph.to_owned(),
                text.add_modifier(Modifier::REVERSED),
            ));
            remaining -= cursor_width;
        } else {
            if glyph_width > remaining {
                break;
            }
            spans.push(Span::styled(glyph.to_owned(), text));
            remaining -= glyph_width;
        }
    }
    Line::from(spans)
}
