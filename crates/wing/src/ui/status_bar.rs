//! Status bar widget — top row showing model, tokens, usage.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Widget;

use crate::config::ThemePalette;

/// Format token count in compact notation.
fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

/// Application state for the status bar.
#[derive(Debug, Clone)]
pub struct StatusData {
    pub model: String,
    /// Active model provider name (None until known / on old gateways).
    pub provider: Option<String>,
    pub total_tokens: i64,
    pub context_window_tokens: i64,
    pub thinking: bool,
    pub reasoning_effort: Option<String>,
    pub yolo: bool,
    pub agent: Option<String>,
    pub session_name: Option<String>,
    /// Current session workdir (session workspace, not the TUI launch dir).
    pub workdir: Option<String>,
    /// Cumulative prompt tokens for the session.
    pub session_prompt_tokens: i64,
    /// Cumulative completion tokens for the session.
    pub session_completion_tokens: i64,
    /// Cumulative cached tokens for the session.
    pub session_cached_tokens: i64,
    /// Whether the gateway connection is active.
    pub connected: bool,
    /// Whether Goal orchestration mode is active.
    pub goal_active: bool,
}

impl Default for StatusData {
    fn default() -> Self {
        Self {
            model: "unknown".into(),
            provider: None,
            total_tokens: 0,
            context_window_tokens: 0,
            thinking: false,
            reasoning_effort: None,
            yolo: false,
            agent: None,
            session_name: None,
            workdir: None,
            session_prompt_tokens: 0,
            session_completion_tokens: 0,
            session_cached_tokens: 0,
            connected: true,
            goal_active: false,
        }
    }
}

impl StatusData {
    /// Apply optional session-state fields from a server event or optimistic update.
    ///
    /// Parameter order mirrors `AppIntent::UpdateSession` field declaration
    /// (`model, agent, title, thinking, reasoning_effort, yolo`) so that
    /// callers destructuring the variant can pass fields through positionally.
    pub fn apply_session_update(
        &mut self,
        model: Option<String>,
        agent: Option<String>,
        title: Option<String>,
        thinking: Option<bool>,
        reasoning_effort: Option<String>,
        yolo: Option<bool>,
    ) {
        if let Some(m) = model {
            self.model = m;
        }
        if let Some(a) = agent {
            self.agent = Some(a);
        }
        if let Some(t) = title {
            self.session_name = Some(t);
        }
        if let Some(t) = thinking {
            self.thinking = t;
        }
        if let Some(e) = reasoning_effort {
            self.reasoning_effort = Some(e);
        }
        if let Some(y) = yolo {
            self.yolo = y;
        }
    }
}

/// Status bar widget — renders a single row at the top.
pub struct StatusBar<'a> {
    data: &'a StatusData,
    /// If true, render the full usage line. If false, omit cumulative details.
    wide: bool,
    palette: &'a ThemePalette,
}

impl<'a> StatusBar<'a> {
    pub fn new(data: &'a StatusData, wide: bool, palette: &'a ThemePalette) -> Self {
        Self {
            data,
            wide,
            palette,
        }
    }
}

impl Widget for StatusBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }

        let d = self.data;
        let dim = Style::default().fg(self.palette.dim);
        let line_y = area.y;

        // Build left spans: " Wing · model [· provider] [think]"
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(
                " Wing",
                Style::default()
                    .fg(self.palette.text)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", dim),
            Span::styled(d.model.clone(), Style::default().fg(self.palette.text)),
        ];

        // Provider name next to the model — hidden when unknown (old gateway).
        if let Some(provider) = d.provider.as_deref().filter(|p| !p.is_empty()) {
            spans.push(Span::styled(" · ", dim));
            spans.push(Span::styled(provider.to_string(), dim));
        }

        if d.thinking {
            let think_label = match &d.reasoning_effort {
                Some(effort) => format!(" think:{}", effort),
                None => " think".to_string(),
            };
            spans.push(Span::styled(think_label, dim));
        }

        if d.goal_active {
            spans.push(Span::styled(
                " [GOAL]",
                Style::default().fg(self.palette.accent),
            ));
        }

        // Build right spans.
        let mut right_spans: Vec<Span<'static>> = Vec::new();

        // Token progress bar.
        if d.context_window_tokens > 0 {
            let ratio = (d.total_tokens as f64 / d.context_window_tokens as f64).min(1.0);
            let filled = (ratio * 10.0) as usize;
            let empty = 10_usize.saturating_sub(filled);

            let bar_color = if ratio >= 0.8 {
                self.palette.danger
            } else if ratio >= 0.5 {
                self.palette.warning
            } else {
                self.palette.success
            };

            right_spans.push(Span::styled(
                "█".repeat(filled),
                Style::default().fg(bar_color),
            ));
            right_spans.push(Span::styled("░".repeat(empty), dim));
            right_spans.push(Span::styled(
                format!(
                    " {}/{}",
                    fmt_tokens(d.total_tokens),
                    fmt_tokens(d.context_window_tokens),
                ),
                dim,
            ));
        } else {
            right_spans.push(Span::styled(
                fmt_tokens(d.total_tokens),
                Style::default().fg(self.palette.success),
            ));
        }

        // Cumulative session usage (wide mode only).
        if self.wide && (d.session_prompt_tokens > 0 || d.session_completion_tokens > 0) {
            right_spans.push(Span::styled(" · ", dim));
            right_spans.push(Span::styled(
                format!("↑{}", fmt_tokens(d.session_prompt_tokens)),
                Style::default().fg(self.palette.dim),
            ));
            right_spans.push(Span::styled(
                format!(" ↓{}", fmt_tokens(d.session_completion_tokens)),
                Style::default().fg(self.palette.dim),
            ));
            if d.session_cached_tokens > 0 && d.session_prompt_tokens > 0 {
                let cache_pct = (d.session_cached_tokens as f64 / d.session_prompt_tokens as f64
                    * 100.0) as u32;
                right_spans.push(Span::styled(
                    format!(" ◎{cache_pct}%"),
                    Style::default().fg(self.palette.dim),
                ));
            }
        }

        // Connection status indicator.
        right_spans.push(Span::styled(" ", dim));
        if d.connected {
            right_spans.push(Span::styled("●", Style::default().fg(self.palette.success)));
        } else {
            right_spans.push(Span::styled(
                "Disconnected",
                Style::default().fg(self.palette.danger),
            ));
        }

        // Calculate right side width.
        let right_width: u16 = right_spans.iter().map(|s| s.width() as u16).sum();

        // Render left spans.
        let mut x = area.x;
        for span in &spans {
            let w = span.width() as u16;
            if x + w > area.right().saturating_sub(right_width + 1) {
                break;
            }
            buf.set_line(x, line_y, &Line::from(span.clone()), w);
            x += w;
        }

        // Render right spans right-aligned.
        let right_x = area.right().saturating_sub(right_width);
        let mut rx = right_x;
        for span in &right_spans {
            let w = span.width() as u16;
            buf.set_line(rx, line_y, &Line::from(span.clone()), w);
            rx += w;
        }
    }
}

/// Per-turn usage data for the input footer.
#[derive(Debug, Clone, Default)]
pub struct TurnUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub tokens_per_sec: f64,
    pub ttft_ms: f64,
}

impl TurnUsage {
    pub fn is_empty(&self) -> bool {
        self.prompt_tokens == 0 && self.completion_tokens == 0
    }

    /// Render as a compact one-liner.
    pub fn to_spans(&self) -> Vec<Span<'static>> {
        if self.is_empty() {
            return Vec::new();
        }
        let dim = Style::default().add_modifier(Modifier::DIM);
        let mut spans: Vec<Span<'static>> = Vec::new();

        spans.push(Span::styled(
            format!("{} in", fmt_tokens(self.prompt_tokens)),
            dim,
        ));
        spans.push(Span::styled(" · ", dim));
        spans.push(Span::styled(
            format!("{} out", fmt_tokens(self.completion_tokens)),
            dim,
        ));
        if self.cached_tokens > 0 && self.prompt_tokens > 0 {
            let hit_rate = self.cached_tokens as f64 / self.prompt_tokens as f64 * 100.0;
            spans.push(Span::styled(" · ", dim));
            spans.push(Span::styled(format!("{hit_rate:.1}% cache"), dim));
        }
        if self.tokens_per_sec > 0.0 {
            spans.push(Span::styled(" · ", dim));
            spans.push(Span::styled(format!("{:.1} t/s", self.tokens_per_sec), dim));
        }
        if self.ttft_ms > 0.0 {
            spans.push(Span::styled(" · ", dim));
            spans.push(Span::styled(format!("{:.0}ms ttft", self.ttft_ms), dim));
        }
        spans
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fmt_tokens() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1000), "1.0k");
        assert_eq!(fmt_tokens(12345), "12.3k");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn test_turn_usage_empty() {
        let usage = TurnUsage::default();
        assert!(usage.is_empty());
        assert!(usage.to_spans().is_empty());
    }

    #[test]
    fn test_turn_usage_renders() {
        let usage = TurnUsage {
            prompt_tokens: 1200,
            completion_tokens: 340,
            cached_tokens: 800,
            tokens_per_sec: 42.5,
            ttft_ms: 320.0,
        };
        let spans = usage.to_spans();
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("1.2k in"), "got: {text}");
        assert!(text.contains("340 out"), "got: {text}");
        // 800/1200 = 66.67%
        assert!(text.contains("66.7% cache"), "got: {text}");
        assert!(text.contains("42.5 t/s"), "got: {text}");
        assert!(text.contains("320ms ttft"), "got: {text}");
    }

    #[test]
    fn test_status_data_default() {
        let data = StatusData::default();
        assert_eq!(data.model, "unknown");
        assert_eq!(data.session_prompt_tokens, 0);
        assert_eq!(data.session_completion_tokens, 0);
    }

    /// Render the status bar into a buffer and return its text.
    fn render_left(data: &StatusData) -> String {
        let area = Rect::new(0, 0, 120, 1);
        let mut buf = Buffer::empty(area);
        StatusBar::new(data, true, &ThemePalette::default()).render(area, &mut buf);
        (0..area.width)
            .map(|x| buf[(x, 0)].symbol())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn test_status_bar_shows_provider_when_known() {
        let data = StatusData {
            model: "deepseek-v4-flash-0731".into(),
            provider: Some("dashscope-openai".into()),
            ..StatusData::default()
        };
        let out = render_left(&data);
        assert!(
            out.contains("deepseek-v4-flash-0731 · dashscope-openai"),
            "provider must render next to the model, got: {out}"
        );
    }

    #[test]
    fn test_status_bar_hides_provider_when_unknown() {
        let data = StatusData {
            model: "gpt-4".into(),
            ..StatusData::default()
        };
        let out = render_left(&data);
        assert!(out.contains("gpt-4"), "got: {out}");
        assert!(
            !out.contains("gpt-4 · "),
            "no provider → no dangling separator, got: {out}"
        );
    }
}
