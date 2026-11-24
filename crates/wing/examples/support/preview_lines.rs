use std::fmt::Write as _;

use ratatui::style::{Color, Modifier};
use ratatui::text::Line;
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

#[allow(dead_code)] // Each example uses one variant.
#[derive(Clone, Copy)]
pub(super) enum Preview {
    Welcome,
    Thinking,
}

/// Encode a line with the preview commands' original SGR and column rules.
pub(super) fn render_line(line: &Line<'_>, width: u16, plain: bool, preview: Preview) -> String {
    let welcome = matches!(preview, Preview::Welcome);
    let margin = usize::from(welcome);
    let mut out = String::new();
    if welcome {
        out.push(' ');
    }
    let mut used = margin;
    for span in &line.spans {
        let mut text = String::new();
        for ch in span.content.chars() {
            let columns = if welcome {
                ch.encode_utf8(&mut [0; 4]).width()
            } else {
                ch.width().unwrap_or(0)
            };
            if used + columns > width as usize {
                break;
            }
            text.push(ch);
            used += columns;
        }
        if text.is_empty() {
            continue;
        }
        let style = span.style;
        if plain {
            // The two colored halves together fill the whole character cell.
            if welcome && style.bg.is_some() {
                text = text.replace(['▀', '▄'], "█");
            }
        } else {
            out.push_str("\x1b[0m");
            if style.add_modifier.contains(Modifier::BOLD) {
                out.push_str("\x1b[1m");
            }
            if !welcome && style.add_modifier.contains(Modifier::DIM) {
                out.push_str("\x1b[2m");
            }
            if let Some(code) = sgr(style.fg) {
                let _ = write!(out, "\x1b[38;{code}m");
            }
            if welcome && let Some(code) = sgr(style.bg) {
                let _ = write!(out, "\x1b[48;{code}m");
            }
        }
        out.push_str(&text);
    }
    if !plain && out.len() > margin {
        out.push_str("\x1b[0m");
    }
    out
}

fn sgr(color: Option<Color>) -> Option<String> {
    let index = match color? {
        Color::Reset => return None,
        Color::Rgb(r, g, b) => return Some(format!("2;{r};{g};{b}")),
        Color::Black => 0,
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::Gray => 7,
        Color::DarkGray => 8,
        Color::LightRed => 9,
        Color::LightGreen => 10,
        Color::LightYellow => 11,
        Color::LightBlue => 12,
        Color::LightMagenta => 13,
        Color::LightCyan => 14,
        Color::White => 15,
        Color::Indexed(_) => 7,
    };
    Some(index.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;
    use ratatui::text::Span;

    #[test]
    fn preserves_preview_line_bytes() {
        let line = Line::from(vec![
            Span::styled(
                "▀中▄",
                Style::default()
                    .fg(Color::Rgb(1, 2, 3))
                    .bg(Color::Rgb(4, 5, 6))
                    .add_modifier(Modifier::BOLD | Modifier::DIM),
            ),
            Span::styled("xyz", Style::default().fg(Color::Indexed(42))),
        ]);
        let width = 6;

        assert_eq!(render_line(&line, width, true, Preview::Welcome), " █中█x");
        assert_eq!(render_line(&line, width, true, Preview::Thinking), "▀中▄xy");
        assert_eq!(
            render_line(&line, width, false, Preview::Welcome),
            " \x1b[0m\x1b[1m\x1b[38;2;1;2;3m\x1b[48;2;4;5;6m▀中▄\x1b[0m\x1b[38;7mx\x1b[0m",
        );
        assert_eq!(
            render_line(&line, width, false, Preview::Thinking),
            "\x1b[0m\x1b[1m\x1b[2m\x1b[38;2;1;2;3m▀中▄\x1b[0m\x1b[38;7mxy\x1b[0m",
        );
    }
}
