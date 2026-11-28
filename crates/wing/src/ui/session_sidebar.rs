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
use crate::shared::session_list::{SessionAge, SessionCandidate};
use crate::shared::session_tree::Crew;
use crate::util::title::dir_label;

const ROW_HEIGHT: u16 = 2;
const HEADER_HEIGHT: u16 = 1;

pub(crate) struct SidebarRow<'a> {
    pub candidate: &'a SessionCandidate,
    pub group: SessionAge,
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
    pub divider: Rect,
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

fn height_for_row(previous: Option<SessionAge>, group: SessionAge) -> u16 {
    ROW_HEIGHT + u16::from(previous != Some(group))
}

pub(crate) fn fitting_rows(area: Rect, groups: impl Iterator<Item = SessionAge>) -> usize {
    let mut remaining = area.height.saturating_sub(HEADER_HEIGHT);
    let mut previous = None;
    groups
        .take_while(|group| {
            let height = height_for_row(previous, *group);
            if height > remaining {
                return false;
            }
            remaining -= height;
            previous = Some(*group);
            true
        })
        .count()
}

pub(crate) struct SidebarContext<'a> {
    pub side: SidebarSide,
    pub resizing: bool,
    pub palette: &'a ThemePalette,
    pub current_id: &'a str,
    pub message: Option<&'a str>,
}

pub(crate) fn render<'a>(
    area: Rect,
    buf: &mut Buffer,
    chrome: SidebarContext<'_>,
    rows: impl Iterator<Item = SidebarRow<'a>>,
) -> SidebarFrame {
    let SidebarContext {
        side,
        resizing,
        palette,
        current_id,
        message,
    } = chrome;
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
    let (divider, divider_style) = if resizing {
        ("┃", Style::default().fg(palette.accent))
    } else {
        ("│", dim)
    };
    for y in area.y..area.bottom() {
        buf[(edge, y)].set_symbol(divider).set_style(divider_style);
    }
    let header = Line::from(vec![
        Span::styled(" 会话", Style::default().fg(palette.text)),
        Span::styled(" · F2", dim),
    ]);
    buf.set_line(content_x, area.y, &header, width);
    if let Some(message) = message {
        let available = usize::from(width).saturating_sub(header.width() + 1);
        let message = elide_to_display_width(message, available);
        let x = area
            .right()
            .saturating_sub(u16::from(side == SidebarSide::Left) + message.width() as u16);
        buf.set_string(x, area.y, message, dim);
    }

    let mut hits = Vec::new();
    let mut y = area.y + HEADER_HEIGHT;
    let mut previous = None;
    for row in rows {
        let height = height_for_row(previous, row.group);
        if height > area.bottom().saturating_sub(y) {
            break;
        }
        if previous != Some(row.group) {
            let title = match row.group {
                SessionAge::Recent => " 最近",
                SessionAge::Yesterday => " 昨天",
                SessionAge::Week => " 一周",
                SessionAge::Older => " 更久",
            };
            buf.set_stringn(content_x, y, title, usize::from(width), dim);
            y += 1;
            previous = Some(row.group);
        }
        let row_area = Rect::new(content_x, y, width, ROW_HEIGHT);
        hits.push(render_row(row_area, buf, palette, row, current_id));
        y += ROW_HEIGHT;
    }
    SidebarFrame {
        area,
        divider: Rect::new(edge, area.y, 1, area.height),
        rows: hits,
    }
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
            SidebarContext {
                side: SidebarSide::Right,
                resizing: false,
                palette: &palette,
                current_id: &candidate.id,
                message: None,
            },
            std::iter::once(SidebarRow {
                candidate: &candidate,
                group: SessionAge::Week,
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
        assert!(text(3).contains("▸ ? "));
        assert!(text(3).contains("… ★"));
        assert!(text(4).contains("▸20 ?4 ●3"));
        assert_eq!(buf[(79, 3)].symbol(), " ");
        assert_eq!(buf[(80, 3)].symbol(), "│");
        assert_eq!(
            frame.hit(Position::new(88, 3)),
            Some(("long-session", RowAction::ToggleChildren))
        );
        assert_eq!(
            frame.hit(Position::new(89, 3)),
            Some(("long-session", RowAction::ToggleChildren))
        );
        assert_eq!(
            frame.hit(Position::new(90, 3)),
            Some(("long-session", RowAction::Open))
        );
        assert_eq!(
            frame.hit(Position::new(88, 4)),
            Some(("long-session", RowAction::Open))
        );
        assert_eq!(frame.hit(Position::new(80, 3)), None);
    }

    #[test]
    fn each_visible_group_consumes_a_heading_row() {
        let same = [SessionAge::Recent, SessionAge::Recent];
        let split = [SessionAge::Recent, SessionAge::Yesterday];
        let area = Rect::new(0, 0, 24, 6);
        assert_eq!(fitting_rows(area, same.into_iter()), 2);
        assert_eq!(fitting_rows(area, split.into_iter()), 1);
        assert_eq!(fitting_rows(area, split.into_iter().rev()), 1);
        assert_eq!(
            fitting_rows(Rect { height: 7, ..area }, split.into_iter()),
            2
        );
    }

    #[test]
    fn empty_area_has_no_rendered_targets() {
        let area = Rect::new(3, 1, 0, 4);
        let palette = ThemePalette::from_config(&AppConfig::default().colors);
        let mut buf = Buffer::empty(area);
        let frame = render(
            area,
            &mut buf,
            SidebarContext {
                side: SidebarSide::Left,
                resizing: false,
                palette: &palette,
                current_id: "",
                message: None,
            },
            std::iter::empty(),
        );
        assert!(frame.area.is_empty());
        assert_eq!(frame.hit(Position::new(3, 2)), None);
    }
}
