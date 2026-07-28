//! TodoMessage — renders TodoWrite tool results.
//!
//! Displays todo items with status icons:
//!   ○ pending task content
//!   ● active task (activeForm)
//!   ✓ completed task

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::config::ThemePalette;

/// A single todo item.
#[derive(Debug, Clone)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
    pub active_form: Option<String>,
}

/// Todo list rendering.
#[derive(Debug, Clone)]
pub struct TodoMessage {
    pub items: Vec<TodoItem>,
}

impl TodoMessage {
    pub fn new(items: Vec<TodoItem>) -> Self {
        Self { items }
    }

    /// Parse from TodoWrite tool_args JSON.
    pub fn from_tool_args(args: &serde_json::Value) -> Option<Self> {
        let todos = args.get("todos")?.as_array()?;
        let items = todos
            .iter()
            .map(|t| TodoItem {
                content: t
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
                status: t
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("pending")
                    .into(),
                active_form: t
                    .get("activeForm")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            })
            .collect();
        Some(Self::new(items))
    }

    /// Render to lines.
    pub fn to_lines(&self, palette: &ThemePalette) -> Vec<Line<'static>> {
        let mut lines = render_todo_items(&self.items, palette);
        lines.push(Line::from(""));
        lines
    }
}

/// Render todo items to lines (without trailing blank line).
///
/// Shared by `TodoMessage::to_lines` (final rendering) and
/// `ToolCallBlock::to_lines` (streaming preview) to prevent drift.
pub fn render_todo_items(items: &[TodoItem], palette: &ThemePalette) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    for item in items {
        let (icon, style, color) = match item.status.as_str() {
            "in_progress" => (
                "●",
                Style::default()
                    .fg(palette.warning)
                    .add_modifier(Modifier::BOLD),
                palette.warning,
            ),
            "completed" => (
                "✓",
                Style::default()
                    .fg(palette.success)
                    .add_modifier(Modifier::DIM),
                palette.success,
            ),
            _ => (
                "○",
                Style::default().fg(palette.dim).add_modifier(Modifier::DIM),
                palette.dim,
            ),
        };

        let display_text = if item.status == "in_progress" {
            item.active_form.as_deref().unwrap_or(&item.content)
        } else {
            &item.content
        };

        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(icon, style),
            Span::raw(" "),
            Span::styled(display_text.to_string(), Style::default().fg(color)),
        ]));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThemePalette;
    use serde_json::json;

    fn p() -> ThemePalette {
        ThemePalette::default()
    }

    #[test]
    fn test_todo_render() {
        let msg = TodoMessage::new(vec![
            TodoItem {
                content: "buy milk".into(),
                status: "completed".into(),
                active_form: None,
            },
            TodoItem {
                content: "fix bug".into(),
                status: "in_progress".into(),
                active_form: Some("Fixing the bug".into()),
            },
            TodoItem {
                content: "write docs".into(),
                status: "pending".into(),
                active_form: None,
            },
        ]);
        let lines = msg.to_lines(&p());
        let text: String = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        // No standalone header — items are rendered directly.
        assert!(text.contains("✓"), "missing completed: {text}");
        assert!(text.contains("●"), "missing in_progress: {text}");
        assert!(text.contains("○"), "missing pending: {text}");
        assert!(
            text.contains("Fixing the bug"),
            "missing active_form: {text}"
        );
    }

    #[test]
    fn test_todo_from_tool_args() {
        let args = json!({
            "todos": [
                {"content": "task1", "status": "pending"},
                {"content": "task2", "status": "in_progress", "activeForm": "Working on task2"}
            ]
        });
        let msg = TodoMessage::from_tool_args(&args).unwrap();
        assert_eq!(msg.items.len(), 2);
        assert_eq!(msg.items[0].content, "task1");
        assert_eq!(msg.items[1].status, "in_progress");
    }

    #[test]
    fn test_todo_from_tool_args_missing() {
        let args = json!({"other": "value"});
        assert!(TodoMessage::from_tool_args(&args).is_none());
    }
}
