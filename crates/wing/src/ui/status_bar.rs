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
    /// Active model's call name (identity — never rendered as-is while a
    /// display label is known).
    pub model: String,
    /// Display label declared for `model` by the gateway config; `None` when
    /// undeclared / on old gateways. Display-only — identity stays `model` +
    /// `provider`.
    pub model_display_name: Option<String>,
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
}

impl Default for StatusData {
    fn default() -> Self {
        Self {
            model: "unknown".into(),
            model_display_name: None,
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
        }
    }
}

impl StatusData {
    /// Model label for display: the gateway-declared display name when present
    /// (non-blank), otherwise the call name.
    ///
    /// The gateway normalizes "no declaration" to `None`; the blank check is
    /// the same defense the `/model` picker applies (`label_for`), so a
    /// whitespace-only label can never blank out the status bar.
    pub fn model_label(&self) -> &str {
        self.model_display_name
            .as_deref()
            .filter(|label| !label.trim().is_empty())
            .unwrap_or(&self.model)
    }

    /// Apply optional session-state fields from a server event or optimistic update.
    ///
    /// Parameter order mirrors `AppIntent::UpdateSession` field declaration
    /// (`model, agent, title, thinking, reasoning_effort, yolo`) so that
    /// callers destructuring the variant can pass fields through positionally.
    /// `model_display_name` rides with `model`: it is only consumed when a new
    /// model value is present, and `None` there means "no declared label"
    /// (display falls back to the call name) — never "keep the old label",
    /// which would describe a model that is no longer active.
    #[allow(clippy::too_many_arguments)] // flat mirror of the session-state fields
    pub fn apply_session_update(
        &mut self,
        model: Option<String>,
        model_display_name: Option<String>,
        agent: Option<String>,
        title: Option<String>,
        thinking: Option<bool>,
        reasoning_effort: Option<String>,
        yolo: Option<bool>,
    ) {
        if let Some(m) = model {
            self.model = m;
            self.model_display_name = model_display_name;
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
        // The model slot renders the declared display label (falling back to
        // the call name) — the raw id is never the first thing the user sees.
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(
                " Wing",
                Style::default()
                    .fg(self.palette.text)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", dim),
            Span::styled(
                d.model_label().to_string(),
                Style::default().fg(self.palette.text),
            ),
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

    /// One entry per number, most useful first.
    ///
    /// The composer's meta rail joins them with ` · ` and drops whole entries
    /// from the tail when the border runs out of room — so the order below is
    /// also the order things disappear in on a narrow terminal. The entries
    /// take the palette's secondary color, exactly like the rail's own rules
    /// and read-outs: one row, one idea of "quiet".
    pub fn items(&self, palette: &crate::config::ThemePalette) -> Vec<Vec<Span<'static>>> {
        if self.is_empty() {
            return Vec::new();
        }
        let dim = Style::default().fg(palette.dim);
        let mut items: Vec<Vec<Span<'static>>> = Vec::new();

        items.push(vec![Span::styled(
            format!("{} in", fmt_tokens(self.prompt_tokens)),
            dim,
        )]);
        items.push(vec![Span::styled(
            format!("{} out", fmt_tokens(self.completion_tokens)),
            dim,
        )]);
        if self.cached_tokens > 0 && self.prompt_tokens > 0 {
            let hit_rate = self.cached_tokens as f64 / self.prompt_tokens as f64 * 100.0;
            items.push(vec![Span::styled(format!("{hit_rate:.1}% cache"), dim)]);
        }
        if self.tokens_per_sec > 0.0 {
            items.push(vec![Span::styled(
                format!("{:.1} t/s", self.tokens_per_sec),
                dim,
            )]);
        }
        if self.ttft_ms > 0.0 {
            items.push(vec![Span::styled(
                format!("{:.0}ms ttft", self.ttft_ms),
                dim,
            )]);
        }
        items
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
        assert!(
            usage
                .items(&crate::config::ThemePalette::default())
                .is_empty()
        );
    }

    #[test]
    fn test_turn_usage_items_are_ordered_by_usefulness() {
        let usage = TurnUsage {
            prompt_tokens: 1200,
            completion_tokens: 340,
            cached_tokens: 800,
            tokens_per_sec: 42.5,
            ttft_ms: 320.0,
        };
        let items: Vec<String> = usage
            .items(&crate::config::ThemePalette::default())
            .iter()
            .map(|item| item.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            items,
            vec![
                "1.2k in",
                "340 out",
                // 800/1200 = 66.67%
                "66.7% cache",
                "42.5 t/s",
                "320ms ttft",
            ]
        );
    }

    #[test]
    fn test_turn_usage_items_skip_what_the_turn_never_reported() {
        let usage = TurnUsage {
            prompt_tokens: 900,
            completion_tokens: 12,
            ..TurnUsage::default()
        };
        let items: Vec<String> = usage
            .items(&crate::config::ThemePalette::default())
            .iter()
            .map(|item| item.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(items, vec!["900 in", "12 out"]);
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

    #[test]
    fn test_status_bar_renders_display_name_not_the_call_name() {
        // The declared display label is the only model text on screen — the
        // call name must not appear (it only ever surfaces in the toast).
        let data = StatusData {
            model: "dfmodel-2026".into(),
            model_display_name: Some("DeepSeek-Flash".into()),
            provider: Some("qoder".into()),
            ..StatusData::default()
        };
        let out = render_left(&data);
        assert!(
            out.contains("DeepSeek-Flash · qoder"),
            "display name must take the model slot, got: {out}"
        );
        assert!(
            !out.contains("dfmodel-2026"),
            "the raw call name must not leak into the status bar, got: {out}"
        );
    }

    #[test]
    fn test_status_bar_display_name_falls_back_when_absent_or_blank() {
        let absent = StatusData {
            model: "plain-model".into(),
            ..StatusData::default()
        };
        assert!(render_left(&absent).contains("plain-model"));

        // Old / other producers may ship a blank label; blank = no label.
        let blank = StatusData {
            model: "plain-model".into(),
            model_display_name: Some("   ".into()),
            ..StatusData::default()
        };
        let out = render_left(&blank);
        let tokens: Vec<&str> = out.split_whitespace().take(3).collect();
        assert_eq!(
            tokens,
            vec!["Wing", "·", "plain-model"],
            "blank label must not render, got: {out}"
        );
    }

    #[test]
    fn test_apply_session_update_keeps_display_name_with_its_model() {
        let mut data = StatusData::default();
        data.apply_session_update(
            Some("dfmodel".into()),
            Some("DeepSeek-Flash".into()),
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(data.model, "dfmodel");
        assert_eq!(data.model_display_name.as_deref(), Some("DeepSeek-Flash"));
        assert_eq!(data.model_label(), "DeepSeek-Flash");
    }

    #[test]
    fn test_apply_session_update_clears_stale_label_on_model_change() {
        let mut data = StatusData::default();
        data.apply_session_update(
            Some("dfmodel".into()),
            Some("DeepSeek-Flash".into()),
            None,
            None,
            None,
            None,
            None,
        );
        // Model changes without a declared label: the old label must not
        // survive and describe the new model.
        data.apply_session_update(Some("plain".into()), None, None, None, None, None, None);
        assert_eq!(data.model, "plain");
        assert_eq!(data.model_display_name, None);
        assert_eq!(data.model_label(), "plain");
    }

    #[test]
    fn test_apply_session_update_without_model_keeps_label() {
        let mut data = StatusData::default();
        data.apply_session_update(
            Some("dfmodel".into()),
            Some("DeepSeek-Flash".into()),
            None,
            None,
            None,
            None,
            None,
        );
        // thinking-only update: model untouched → label untouched.
        data.apply_session_update(None, None, None, None, Some(true), None, None);
        assert_eq!(data.model, "dfmodel");
        assert_eq!(data.model_display_name.as_deref(), Some("DeepSeek-Flash"));
    }
}
