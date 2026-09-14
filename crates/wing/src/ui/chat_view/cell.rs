//! The chat view's content unit: [`ChatCell`] and its rendering.
//!
//! A cell owns **what it looks like** (its `to_lines` / `render_lines` output)
//! and nothing else: no scroll state, no viewport geometry, no frame state.
//! Height and line caching live one layer up, in
//! [`CachedCell`](crate::ui::cached_cell::CachedCell); wrapping into screen
//! rows is the widget's job (`super::viewport`).
//!
//! [`PendingMessage`] lives here too: it is the cell wrapper of a
//! sent-but-not-yet-accepted user message, i.e. another content state, not a
//! viewport concern.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;
use unicode_width::UnicodeWidthStr;

use crate::app::model_panel::ModelPanel;
use crate::config::ThemePalette;
use crate::render::Renderable;
use crate::render::markdown::ComposedLines;
use crate::render::markdown::compose_lines;
use crate::render::markdown::links::CELL_PREFIX_WIDTH;
use crate::render::markdown::render_markdown_lines;
use crate::render::markdown::render_plain;
use crate::render::renderable::CellContext;
use crate::ui::cached_cell::CachedCell;
use crate::ui::cells::ask_msg::AskMessage;
use crate::ui::cells::diff_view::DiffView;
use crate::ui::cells::model_picker::model_picker_lines;
use crate::ui::cells::thinking::ThinkingBlock;
use crate::ui::cells::todo_msg::TodoMessage;
use crate::ui::cells::tool_call::ToolCallBlock;

/// A single cell in the chat view.
#[derive(Debug, Clone)]
pub enum ChatCell {
    /// User message.
    UserMessage(String),
    /// User message awaiting model acceptance — rendered in the pending
    /// area below all committed cells, dimmed to signal "queued, not yet
    /// fed to the model". Promoted to `UserMessage` on
    /// `user_message_accepted`, or demoted to `DiscardedUserMessage` when
    /// an interrupt discards it.
    PendingUserMessage(String),
    /// User message discarded by an interrupt before reaching the model —
    /// dimmed + struck through to signal "never sent".
    DiscardedUserMessage(String),
    /// Assistant text (may be accumulated from streaming TextEvents).
    AssistantMessage(String),
    /// System message.
    SystemMessage(String),
    /// Warning message — a `notice` event rendered as a yellow system
    /// message. Unlike `ErrorMessage` it does **not** imply the turn ended.
    WarningMessage(String),
    /// Error message.
    ErrorMessage(String),
    /// Reasoning/thinking block (always expanded).
    Thinking(ThinkingBlock),
    /// Tool invocation + result.
    ToolCall(ToolCallBlock),
    /// File diff view.
    Diff(DiffView),
    /// Todo list.
    Todo(TodoMessage),
    /// Agent question.
    Ask(AskMessage),
    /// `/model` picker (transient — removed when applied or closed).
    ModelPicker(ModelPanel),
    /// ReAct loop separator.
    Separator,
    /// Goal orchestration separator (marks agent role + round).
    GoalSeparator {
        role: crate::app::goal::GoalRole,
        round: u32,
    },
}

impl ChatCell {
    /// Render this cell to lines **plus their markdown link spans**.
    ///
    /// Cells without markdown links (everything but assistant messages and
    /// thinking blocks) return [`ComposedLines::plain`] — the spans exist so
    /// the widget can inject OSC8 hyperlinks and hit-test clicks without ever
    /// re-deriving what the renderer produced (see `tui-link-open`).
    pub fn render_lines(&self, width: u16, ctx: &CellContext<'_>) -> ComposedLines {
        match self {
            Self::AssistantMessage(text) => assistant_message_lines(text, width, ctx.palette),
            Self::Thinking(block) => block.render_lines(ctx.palette, ctx.thinking_mode, width),
            _ => ComposedLines::plain(self.to_lines(width, ctx)),
        }
    }

    /// Render this cell to lines, width-aware for full-width elements.
    pub fn to_lines(&self, width: u16, ctx: &CellContext<'_>) -> Vec<Line<'static>> {
        let palette = ctx.palette;
        match self {
            Self::UserMessage(text) => {
                let style = Style::default().bg(palette.surface).fg(palette.text);
                Self::user_message_lines(text, style)
            }
            Self::PendingUserMessage(text) => {
                // Queued, not yet accepted by the model — dimmed.
                let style = Style::default().bg(palette.surface).fg(palette.dim);
                Self::user_message_lines(text, style)
            }
            Self::DiscardedUserMessage(text) => {
                // Discarded by an interrupt before reaching the model —
                // dimmed + struck through.
                let style = Style::default()
                    .bg(palette.surface)
                    .fg(palette.dim)
                    .add_modifier(Modifier::CROSSED_OUT);
                Self::user_message_lines(text, style)
            }
            Self::AssistantMessage(text) => {
                assistant_message_lines(text, width, palette).into_lines()
            }
            Self::SystemMessage(text) => {
                let label = Style::default().fg(palette.accent);
                let body = Style::default().fg(palette.text).italic();
                let mut lines = vec![Line::from(Span::styled("⦁ system", label))];
                for line in render_plain(text) {
                    lines.push(Span::styled(line.to_string(), body).into());
                }
                lines.push(Line::from(""));
                lines
            }
            Self::WarningMessage(text) => {
                // A notice (e.g. "retrying in 6s") — yellow, but explicitly not
                // an error: the turn is still running.
                let warning = Style::default().fg(palette.warning);
                let mut lines = vec![Line::from(Span::styled("⦁ warning", warning.bold()))];
                for line in render_plain(text) {
                    lines.push(Span::styled(line.to_string(), warning).into());
                }
                lines.push(Line::from(""));
                lines
            }
            Self::ErrorMessage(text) => {
                let danger = Style::default().fg(palette.danger);
                let mut lines = vec![Line::from(Span::styled("⦁ error", danger.bold()))];
                for line in render_plain(text) {
                    lines.push(Span::styled(line.to_string(), danger).into());
                }
                lines.push(Line::from(""));
                lines
            }
            Self::Thinking(block) => block.to_lines(palette, ctx.thinking_mode, width),
            Self::ToolCall(block) => block.to_lines(palette, ctx.layout.tool_output_max),
            Self::Diff(view) => view.to_lines(palette, width),
            Self::Todo(msg) => msg.to_lines(palette),
            Self::Ask(msg) => msg.to_lines(palette, width),
            Self::ModelPicker(panel) => model_picker_lines(panel, palette),
            Self::Separator => {
                let sep = "─".repeat(width as usize);
                vec![Line::from(Span::styled(
                    sep,
                    Style::default().fg(palette.dim),
                ))]
            }
            Self::GoalSeparator { role, round } => {
                let label = format!(" {} {} · Round {} ", role.icon(), role.label(), round);
                // Display width (not char count) — emoji like 🔍 are 2 columns.
                let label_w = UnicodeWidthStr::width(label.as_str());
                let dash_total = (width as usize).saturating_sub(label_w);
                let left = dash_total / 2;
                let right = dash_total - left;
                let text = format!("{}{}{}", "─".repeat(left), label, "─".repeat(right));
                vec![
                    Line::from(""),
                    Line::from(Span::styled(text, Style::default().fg(palette.dim))),
                    Line::from(""),
                ]
            }
        }
    }

    /// Shared rendering for the three user-message cell states (normal /
    /// pending / discarded) — plain text lines under the given style.
    fn user_message_lines(text: &str, style: Style) -> Vec<Line<'static>> {
        render_plain(text)
            .iter()
            .map(|line| Line::from(Span::styled(line.to_string(), style)))
            .collect()
    }
}

/// Assistant message lines: markdown prefixed with `⦁ ` / `  `, plus the link
/// spans of every rendered line (shifted by the 2-column prefix).
///
/// Reserves 2 columns for the prefix so tables balance to fit and downstream
/// wrapping never breaks a row.
fn assistant_message_lines(text: &str, width: u16, palette: &ThemePalette) -> ComposedLines {
    let md_width = Some(width.saturating_sub(2));
    let md_lines = render_markdown_lines(text, md_width, palette);
    let bullet_style = Style::default().fg(palette.text);
    let mut composed = compose_lines(
        &md_lines,
        CELL_PREFIX_WIDTH,
        |i| {
            if i == 0 {
                Span::styled("⦁ ", bullet_style)
            } else {
                Span::raw("  ")
            }
        },
        |_kind, style| style,
    );
    composed.push_blank();
    composed
}

impl Renderable for ChatCell {
    fn render(&self, area: Rect, buf: &mut Buffer, ctx: &CellContext<'_>) {
        Paragraph::new(self.to_lines(area.width, ctx))
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    /// Wrap-aware line count — the single source of truth for cell height.
    fn desired_height(&self, width: u16, ctx: &CellContext<'_>) -> usize {
        // UserMessage: text rendered in inset area (2 left, 1 top, 1 bottom padding)
        if matches!(
            self,
            Self::UserMessage(_) | Self::PendingUserMessage(_) | Self::DiscardedUserMessage(_)
        ) {
            let text_width = width.saturating_sub(3);
            return Paragraph::new(self.to_lines(width, ctx))
                .wrap(Wrap { trim: false })
                .line_count(text_width)
                + 2; // top + bottom padding
        }
        Paragraph::new(self.to_lines(width, ctx))
            .wrap(Wrap { trim: false })
            .line_count(width)
    }
}

/// A user message that has been sent but not yet accepted by the model.
///
/// Rendered in the pending area below all committed cells ("stuck at the
/// bottom") until `user_message_accepted` promotes it into history.
pub struct PendingMessage {
    /// ClientRequest.request_id — correlates with UserMessageAccepted.
    pub request_id: String,
    pub(crate) cell: CachedCell, // PendingUserMessage(content)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{make_ctx, test_ctx};
    use super::*;

    #[test]
    fn test_renderable_desired_height() {
        let cell = ChatCell::UserMessage("hello".into());
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);
        let h = cell.desired_height(80, &ctx);
        assert!(h > 0);
        // " hello \n\n" = 2 lines at any reasonable width
        assert!(h >= 2);
    }

    #[test]
    fn test_user_message_no_you_prefix() {
        let cell = ChatCell::UserMessage("hello world".into());
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);
        let lines = cell.to_lines(80, &ctx);
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text.contains("⦁ You"),
            "should not contain '⦁ You': {text}"
        );
        assert!(
            text.contains("hello world"),
            "should contain message: {text}"
        );
    }

    #[test]
    fn test_user_message_state_lines_use_distinct_styles() {
        let (palette, layout) = test_ctx();
        let ctx = make_ctx(&palette, &layout);

        let normal = ChatCell::UserMessage("m".into()).to_lines(80, &ctx);
        let pending = ChatCell::PendingUserMessage("m".into()).to_lines(80, &ctx);
        let discarded = ChatCell::DiscardedUserMessage("m".into()).to_lines(80, &ctx);

        // Pending is dimmed; discarded is dimmed + struck through; normal is neither.
        let style_of = |lines: &Vec<Line<'static>>| lines[0].spans[0].style;
        assert_eq!(style_of(&pending).fg, Some(palette.dim));
        assert_eq!(style_of(&discarded).fg, Some(palette.dim));
        assert!(
            style_of(&discarded)
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        assert!(
            !style_of(&pending)
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        assert_eq!(style_of(&normal).fg, Some(palette.text));
    }
}
