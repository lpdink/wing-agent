//! Status bar widget — top row showing model, tokens, usage, session identity.
//!
//! The bar declares the frame's only top-row pointer targets: the session id
//! (click = copy) and the pin star (click = toggle). Their hit regions are
//! recorded while drawing (see [`StatusBarRegions`]) and measured the way the
//! layout measures everything else — `unicode-width`, which resolves the
//! East-Asian *Ambiguous* glyphs (`…` `☆` `★` `·` …) to one column.
//!
//! **Known boundary**: a terminal configured to render ambiguous-width
//! characters as two columns would draw those glyphs one column wider than we
//! account for, so a pointer would land one cell right of the recorded region
//! (the whole frame is misaligned there — every box-drawing border in the TUI
//! has the same assumption). We keep the region equal to the drawn glyph
//! rather than padding it, so the contract stays "a region covers exactly the
//! cells it was drawn in".

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
    /// 当前会话是否被 pin（`pin` 标签；pin 是前端约定，后端零感知——
    /// 见 `crate::shared::pinning`）。
    pub pinned: bool,
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
            pinned: false,
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

/// 状态栏上的两处**可点区域**（本帧实际绘制出来的位置；没画出来 = `None`）。
///
/// 命中区由绘制本身回填（同滚动条的"记录本帧事实"契约）：鼠标事件在两帧
/// 之间到达，点的是**用户正看着的那一帧**——重算一遍布局就会在数据于两帧
/// 之间变化时指向别处。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatusBarRegions {
    /// 会话 ID 段（点击 = 复制完整 ID）。
    pub session_id: Option<Rect>,
    /// pin 星标（点击 = 切换 pin）。
    pub star: Option<Rect>,
}

/// 身份段里认领指针的格子（其余 span 不认领）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    SessionId,
    Star,
}

/// 窄终端的会话 ID 退化形态：尾部 8 位（唯一的那段 hex）+ 省略号前缀。
///
/// 完整 ID（`YYYYMMDD-HHMMSS-<8hex>`，24 字符）只在宽终端画全；点击复制
/// 拿到的**永远**是完整 ID，显示形态只是空间妥协。
fn compact_session_id(id: &str) -> String {
    let tail: String = id
        .chars()
        .skip(id.chars().count().saturating_sub(8))
        .collect();
    format!("…{tail}")
}

/// Status bar widget — renders a single row at the top.
pub struct StatusBar<'a> {
    data: &'a StatusData,
    /// 当前会话 ID（空串 = 未知：身份段整段不渲染）。
    session_id: &'a str,
    /// If true, render the full usage line. If false, omit cumulative details.
    wide: bool,
    palette: &'a ThemePalette,
    /// 命中区回填：绘制时写入本帧的真实位置（见 [`StatusBarRegions`]）。
    regions: &'a mut StatusBarRegions,
}

impl<'a> StatusBar<'a> {
    pub fn new(
        data: &'a StatusData,
        session_id: &'a str,
        wide: bool,
        palette: &'a ThemePalette,
        regions: &'a mut StatusBarRegions,
    ) -> Self {
        Self {
            data,
            session_id,
            wide,
            palette,
            regions,
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

        // Build the left chain: " Wing · <session id> ☆ · model [· provider] [think]".
        //
        // 会话身份（ID + pin 星标）紧跟在品牌之后、模型之前：星标是 unpin 的
        // **唯一入口**，不能被"模型名太长"这类内容挤掉——链条从左到右在容量
        // 用尽处截断，越靠前的越先保住。身份段是 dim 的引用材料；模型仍是第
        // 一个亮色内容。
        let mut spans: Vec<(Span<'static>, Option<Slot>)> = vec![
            (
                Span::styled(
                    " Wing",
                    Style::default()
                        .fg(self.palette.text)
                        .add_modifier(Modifier::BOLD),
                ),
                None,
            ),
            (Span::styled(" · ", dim), None),
        ];
        if !self.session_id.is_empty() {
            // 宽终端画完整 ID，窄终端退化尾 8 位（点击复制的永远是完整 ID）。
            let id_text = if self.wide {
                self.session_id.to_string()
            } else {
                compact_session_id(self.session_id)
            };
            let (star_glyph, star_style) = if d.pinned {
                ("★", Style::default().fg(self.palette.warning))
            } else {
                ("☆", dim)
            };
            spans.push((Span::styled(id_text, dim), Some(Slot::SessionId)));
            spans.push((Span::styled(" ", dim), None));
            spans.push((Span::styled(star_glyph, star_style), Some(Slot::Star)));
            spans.push((Span::styled(" · ", dim), None));
        }
        // The model slot renders the declared display label (falling back to
        // the call name) — the raw id is never the first thing the user sees.
        spans.push((
            Span::styled(
                d.model_label().to_string(),
                Style::default().fg(self.palette.text),
            ),
            None,
        ));

        // Provider name next to the model — hidden when unknown (old gateway).
        if let Some(provider) = d.provider.as_deref().filter(|p| !p.is_empty()) {
            spans.push((Span::styled(" · ", dim), None));
            spans.push((Span::styled(provider.to_string(), dim), None));
        }

        if d.thinking {
            let think_label = match &d.reasoning_effort {
                Some(effort) => format!(" think:{}", effort),
                None => " think".to_string(),
            };
            spans.push((Span::styled(think_label, dim), None));
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

        // 命中区是**本帧事实**：先清空，画出来才回填（见 [`StatusBarRegions`]）。
        *self.regions = StatusBarRegions::default();

        // Render the left chain — capacity runs out at the right edge of the
        // band the right cluster leaves; the first span that does not fit ends
        // the chain (positions stay stable, nothing jumps around).
        let limit = area.right().saturating_sub(right_width + 1);
        let mut x = area.x;
        for (span, slot) in &spans {
            let w = span.width() as u16;
            if x + w > limit {
                break;
            }
            buf.set_line(x, line_y, &Line::from(span.clone()), w);
            match slot {
                Some(Slot::SessionId) => {
                    self.regions.session_id = Some(Rect::new(x, line_y, w, 1));
                }
                Some(Slot::Star) => {
                    self.regions.star = Some(Rect::new(x, line_y, w, 1));
                }
                None => {}
            }
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

    /// The full 24-char session id the fixtures use.
    const SID: &str = "20261005-213012-ab12cd34";

    /// Render the status bar into a buffer and return (one entry per column,
    /// hit regions). Every glyph the bar uses is single-width, so a region's
    /// `x..x+width` slices the very characters it covers.
    fn render_bar(
        data: &StatusData,
        session_id: &str,
        width: u16,
    ) -> (Vec<String>, StatusBarRegions) {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        let mut regions = StatusBarRegions::default();
        StatusBar::new(
            data,
            session_id,
            width >= 100,
            &ThemePalette::default(),
            &mut regions,
        )
        .render(area, &mut buf);
        let text = (0..area.width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect::<Vec<_>>();
        (text, regions)
    }

    /// The characters a region covers, joined.
    fn slice(text: &[String], rect: Rect) -> String {
        text[rect.x as usize..(rect.x + rect.width) as usize].join("")
    }

    /// Render at the standard width and return the text as one string.
    fn render_text(data: &StatusData) -> String {
        render_bar(data, "", 120).0.join("")
    }

    #[test]
    fn test_status_bar_shows_provider_when_known() {
        let data = StatusData {
            model: "deepseek-v4-flash-0731".into(),
            provider: Some("dashscope-openai".into()),
            ..StatusData::default()
        };
        let out = render_text(&data);
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
        let out = render_text(&data);
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
        let out = render_text(&data);
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
        assert!(render_text(&absent).contains("plain-model"));

        // Old / other producers may ship a blank label; blank = no label.
        let blank = StatusData {
            model: "plain-model".into(),
            model_display_name: Some("   ".into()),
            ..StatusData::default()
        };
        let out = render_text(&blank);
        let tokens: Vec<&str> = out.split_whitespace().take(3).collect();
        assert_eq!(
            tokens,
            vec!["Wing", "·", "plain-model"],
            "blank label must not render, got: {out}"
        );
    }

    #[test]
    fn test_status_bar_renders_the_full_session_id_when_wide() {
        let (text, regions) = render_bar(&StatusData::default(), SID, 120);
        let id = regions.session_id.expect("id region recorded");
        assert_eq!(id.y, 0);
        assert_eq!(slice(&text, id), SID, "wide bar shows the whole id");
        assert!(id.x > 5, "the id follows the brand: {text:?}");
    }

    #[test]
    fn test_status_bar_compacts_the_session_id_on_a_narrow_terminal() {
        let (text, regions) = render_bar(&StatusData::default(), SID, 80);
        let id = regions.session_id.expect("id region recorded");
        assert_eq!(slice(&text, id), "…ab12cd34", "narrow bar keeps the tail");
        assert!(
            !text.contains(&SID.to_string()),
            "the full id does not fit: {text:?}"
        );
    }

    #[test]
    fn test_status_bar_star_reflects_pin_state() {
        let (text, regions) = render_bar(&StatusData::default(), SID, 120);
        let star = regions.star.expect("star region recorded");
        assert_eq!(slice(&text, star), "☆", "unpinned is a hollow star");
        assert!(
            star.x > regions.session_id.unwrap().x,
            "the star sits right after the id"
        );

        let pinned = StatusData {
            pinned: true,
            ..StatusData::default()
        };
        let (text, regions) = render_bar(&pinned, SID, 120);
        let star = regions.star.unwrap();
        assert_eq!(slice(&text, star), "★", "pinned is a filled star");
        assert!(!text.contains(&"☆".to_string()));
    }

    #[test]
    fn test_status_bar_identity_survives_a_long_model_name() {
        // 星标是 unpin 的唯一入口，不能被模型名挤掉：身份段在链条开头，
        // 容量用尽时先牺牲的是模型之后的内容。
        let data = StatusData {
            model: "a-very-long-model-name-that-eats-the-row".into(),
            provider: Some("some-provider".into()),
            total_tokens: 123_456,
            context_window_tokens: 200_000,
            ..StatusData::default()
        };
        let (text, regions) = render_bar(&data, SID, 80);
        let id = regions.session_id.expect("id drawn");
        let star = regions.star.expect("star drawn");
        assert_eq!(slice(&text, id), "…ab12cd34");
        assert_eq!(slice(&text, star), "☆");
        assert!(
            !text.join("").contains("a-very-long-model-name"),
            "the model is what got dropped: {text:?}"
        );
    }

    #[test]
    fn test_a_region_exists_iff_its_content_was_drawn() {
        // 命中区的唯一判据是"本帧真的画出来了"：扫描宽度谱，星标与 ID 的
        // 命中区必须在且仅在文本里能看到它们时存在——没画出来的东西不可点。
        for width in 20..=120 {
            let (text, regions) = render_bar(&StatusData::default(), SID, width);
            let joined = text.join("");
            assert_eq!(
                regions.star.is_some(),
                joined.contains('☆'),
                "width {width}: {joined:?}"
            );
            match regions.session_id {
                Some(id) => {
                    let drawn = slice(&text, id);
                    assert!(
                        drawn == SID || drawn == "…ab12cd34",
                        "width {width}: region must cover the id text, got {drawn:?}"
                    );
                }
                None => assert!(
                    !joined.contains("ab12cd34"),
                    "width {width}: no region but the id shows — {joined:?}"
                ),
            }
        }
    }

    #[test]
    fn test_status_bar_without_a_session_id_has_no_identity_tail() {
        // 空 id = 不知道身份 → 整段不渲染（而不是画一个光秃秃的星标）。
        let (text, regions) = render_bar(&StatusData::default(), "", 120);
        assert!(!text.iter().any(|c| c == "☆" || c == "★"), "{text:?}");
        assert_eq!(regions, StatusBarRegions::default());
        assert!(text.join("").contains("Wing · unknown"), "{text:?}");
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
