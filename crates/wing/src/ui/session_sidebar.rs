//! Session sidebar painting and the hit targets it puts on screen.

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::session_row::{PIN_MARK, PIN_WIDTH, UNTITLED, crew_badges, format_last_active};
use crate::config::{SidebarSide, ThemePalette};
use crate::protocol::SessionStatus;
use crate::render::markdown::elide_to_display_width;
use crate::shared::session_list::SessionCandidate;
use crate::shared::session_tree::Crew;
use crate::util::title::dir_label;

const ROW_HEIGHT: u16 = 2;
const HEADER_HEIGHT: u16 = 1;

pub(crate) struct SidebarRow<'a> {
    pub candidate: &'a SessionCandidate,
    pub depth: usize,
    pub crew: Crew,
    pub collapsed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowAction {
    Open,
    ToggleChildren,
}

struct RowHit {
    area: Rect,
    session_id: String,
    disclosure: Option<Rect>,
}

#[derive(Default)]
pub(crate) struct SidebarFrame {
    pub area: Rect,
    rows: Vec<RowHit>,
}

impl SidebarFrame {
    pub fn hit(&self, position: Position) -> Option<(&str, RowAction)> {
        let hit = self.rows.iter().find(|hit| hit.area.contains(position))?;
        let action = if hit.disclosure.is_some_and(|area| area.contains(position)) {
            RowAction::ToggleChildren
        } else {
            RowAction::Open
        };
        Some((&hit.session_id, action))
    }
}

pub(crate) fn capacity(area: Rect) -> usize {
    usize::from(area.height.saturating_sub(HEADER_HEIGHT) / ROW_HEIGHT)
}

pub(crate) fn render<'a>(
    area: Rect,
    buf: &mut Buffer,
    side: SidebarSide,
    palette: &ThemePalette,
    current_id: &str,
    message: Option<&str>,
    rows: impl Iterator<Item = SidebarRow<'a>>,
) -> SidebarFrame {
    if area.width < 2 || area.height == 0 {
        return SidebarFrame::default();
    }
    let edge = match side {
        SidebarSide::Left => area.right() - 1,
        SidebarSide::Right => area.x,
    };
    let content_x = area.x + u16::from(side == SidebarSide::Right);
    let width = area.width - 1;
    let dim = Style::default().fg(palette.dim);
    for y in area.y..area.bottom() {
        buf[(edge, y)].set_symbol("│").set_style(dim);
    }
    let header = Line::from(vec![
        Span::styled(" 会话", Style::default().fg(palette.text)),
        Span::styled(" · F2", dim),
    ]);
    buf.set_line(content_x, area.y, &header, width);
    if let Some(message) = message {
        let x = content_x + width.saturating_sub(message.width() as u16 + 1);
        buf.set_string(x, area.y, message, dim);
    }

    let rows = rows
        .take(capacity(area))
        .enumerate()
        .map(|(index, row)| {
            let y = area.y + HEADER_HEIGHT + index as u16 * ROW_HEIGHT;
            let row_area = Rect::new(content_x, y, width, ROW_HEIGHT);
            render_row(row_area, buf, palette, row, current_id)
        })
        .collect();
    SidebarFrame { area, rows }
}

fn render_row(
    area: Rect,
    buf: &mut Buffer,
    palette: &ThemePalette,
    row: SidebarRow<'_>,
    current_id: &str,
) -> RowHit {
    let candidate = row.candidate;
    let selected = candidate.id == current_id;
    let base = if selected {
        Style::default().bg(palette.surface)
    } else {
        Style::default()
    };
    let title_style = if selected {
        base.fg(palette.accent).add_modifier(Modifier::BOLD)
    } else {
        base.fg(palette.text)
    };
    let dim = base.fg(palette.dim);
    buf.set_style(area, base);

    let indent = row.depth.min(3) * 2 + 1;
    let expandable = row.crew.descendants > 0;
    let disclosure = match (expandable, row.collapsed) {
        (true, true) => "▸",
        (true, false) => "▾",
        (false, _) => " ",
    };
    let status = candidate.parsed_status().unwrap_or(SessionStatus::Inactive);
    let title = candidate.title.lines().next().unwrap_or_default().trim();
    let title = if title.is_empty() { UNTITLED } else { title };
    let pin_width = usize::from(candidate.pinned) * PIN_WIDTH;
    let text_indent = indent + 4;
    let width = usize::from(area.width);
    let title_width = width.saturating_sub(text_indent + pin_width + 1);
    let mut title_spans = vec![
        Span::styled(" ".repeat(indent), base),
        Span::styled(disclosure, dim),
        Span::styled(" ", base),
        Span::styled(status.icon(), base.fg(status.color(palette))),
        Span::styled(" ", base),
        Span::styled(elide_to_display_width(title, title_width), title_style),
    ];
    if candidate.pinned {
        title_spans.push(Span::styled(PIN_MARK, base.fg(palette.warning)));
    }
    buf.set_line(area.x, area.y, &Line::from(title_spans), area.width);

    let auxiliary_width = width.saturating_sub(text_indent + 1);
    let time = format_last_active(&candidate.last_interaction);
    let mut info = vec![Span::styled(" ".repeat(text_indent), base)];
    let (badges, badge_width) = if row.collapsed {
        crew_badges(row.crew, palette, base, auxiliary_width)
    } else {
        (Vec::new(), 0)
    };
    let time_width = time.width() + usize::from(!time.is_empty()) * 3;
    let budget = auxiliary_width.saturating_sub(badge_width + usize::from(badge_width > 0));
    let workspace = dir_label(Some(&candidate.workspace));
    let label = workspace.as_deref().unwrap_or(&candidate.id);
    let show_time = !time.is_empty() && budget > time_width;
    let label_width = budget - if show_time { time_width } else { 0 };
    info.push(Span::styled(
        elide_to_display_width(label, label_width),
        dim,
    ));
    if show_time {
        info.push(Span::styled(format!(" · {time}"), dim));
    }
    if badge_width > 0 {
        if label_width > 0 {
            info.push(Span::styled(" ", base));
        }
        info.extend(badges);
    }
    buf.set_line(area.x, area.y + 1, &Line::from(info), area.width);

    RowHit {
        area,
        session_id: candidate.id.clone(),
        disclosure: expandable.then(|| Rect::new(area.x + indent as u16, area.y, 2, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn dense_rows_keep_status_pin_and_disclosure_inside_the_sidebar() {
        let candidate = SessionCandidate {
            id: "long-session".into(),
            parent: None,
            title: "非常长的会话标题".repeat(5),
            workspace: format!("/projects/{}", "工作区".repeat(10)),
            status: SessionStatus::Waiting.as_str().into(),
            last_interaction: "2026-01-01T14:30:00".into(),
            pinned: true,
            pin_added_at: None,
        };
        let palette = ThemePalette::from_config(&AppConfig::default().colors);
        let mut buf = Buffer::empty(Rect::new(0, 0, 110, 6));
        let area = Rect::new(80, 1, 30, 5);
        let frame = render(
            area,
            &mut buf,
            SidebarSide::Right,
            &palette,
            &candidate.id,
            None,
            std::iter::once(SidebarRow {
                candidate: &candidate,
                depth: 16,
                crew: Crew {
                    descendants: 20,
                    waiting: 4,
                    working: 3,
                    activity: None,
                },
                collapsed: true,
            }),
        );
        let text = |y| {
            (area.x..area.right())
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        };
        assert!(text(2).contains("▸ ? "));
        assert!(text(2).contains("… ★"));
        assert!(text(3).contains("▸20 ?4 ●3"));
        assert_eq!(buf[(79, 2)].symbol(), " ");
        assert_eq!(buf[(80, 2)].symbol(), "│");
        assert_eq!(
            frame.hit(Position::new(88, 2)),
            Some(("long-session", RowAction::ToggleChildren))
        );
        assert_eq!(
            frame.hit(Position::new(89, 2)),
            Some(("long-session", RowAction::ToggleChildren))
        );
        assert_eq!(
            frame.hit(Position::new(90, 2)),
            Some(("long-session", RowAction::Open))
        );
        assert_eq!(
            frame.hit(Position::new(88, 3)),
            Some(("long-session", RowAction::Open))
        );
        assert_eq!(frame.hit(Position::new(80, 2)), None);
    }

    #[test]
    fn empty_area_has_no_rendered_targets() {
        let area = Rect::new(3, 1, 0, 4);
        let palette = ThemePalette::from_config(&AppConfig::default().colors);
        let mut buf = Buffer::empty(area);
        let frame = render(
            area,
            &mut buf,
            SidebarSide::Left,
            &palette,
            "",
            None,
            std::iter::empty(),
        );
        assert!(frame.area.is_empty());
        assert_eq!(frame.hit(Position::new(3, 2)), None);
    }
}
