//! Model picker cell — the `/model` card rendered inside the chat view.
//!
//! The picker lives in the transcript like the Ask panel and leaves no record:
//! the cell is removed the moment the user applies a model or closes the
//! panel. It is a **card** — rounded border, title bar, footer — because the
//! panel is a deliberate, transient surface, not a line of output:
//!
//! ```text
//! ╭─ Model ─────────────────────── ● DeepSeek-Flash · MacMachine ─╮
//! │                                                               │
//! │  dashscope    MacMachine    magpie                  4 models  │
//! │               ▔▔▔▔▔▔▔▔▔▔                                      │
//! │                                                               │
//! │  ❯ DeepSeek-Flash                    500k  vision  ● current  │
//! │    GLM-5.3                           500k  vision             │
//! │    Qwen3.8-max                       500k  vision             │
//! │    Qwen3.8-Flash                     500k  vision             │
//! │                                                               │
//! │  id dfmodel · keep 100k                                       │
//! │  ↑↓ move · ←→ provider · Enter apply · Esc close       1 / 4  │
//! ╰───────────────────────────────────────────────────────────────╯
//! ```
//!
//! One question per zone:
//!
//! - **title bar** — *where am I?* the session's active model and its
//!   provider (the `●` the rows reuse);
//! - **tab strip** — *which shelf?* the provider tabs, the active one
//!   underlined, the page's model count on the right;
//! - **rows** — *what are the options and how do they differ?* the display
//!   label plus the declared data columns (context window, vision) and a
//!   `● current` badge on the model the session is on;
//! - **detail line** — *what exactly is the highlighted row?* the declared
//!   `id` (the reference word configs and requests use), the compaction
//!   reserve, and the description when declared. It follows the cursor.
//!
//! # Geometry (product decisions)
//!
//! - The card hugs its content, capped at [`CARD_MAX_WIDTH`] and by the cell
//!   width minus a 2-column right margin (a border flush against the content
//!   edge reads as a layout bug, not a full-bleed panel).
//! - Column widths come from the **whole catalog**, never from the active
//!   page: switching providers never resizes the card, so the eye keeps its
//!   landmarks.
//! - The footer (key hints + position) takes part in the width maths, so a
//!   hint never wraps into the border.
//! - Degradation ladder, by the cell's available width:
//!   - `>= 64` **full** — detail line, `● current` badge, ctx + vision columns;
//!   - `42..64` **compact** — no detail line, bare `●`, shorter hints;
//!   - `< 42` **rail** — no border at all: a title line, the list, one hint
//!     line (a border that squeezes the rows is worse than no border);
//!   - a constrained card drops data columns (`vision` first, then ctx) until
//!     the label column can breathe ([`LABEL_MIN_WIDTH`]).
//! - Long labels clip with `…`; they never push the data columns around.
//!
//! Colors: `accent` marks the interactive state (active tab, cursor), `success`
//! the `●` current marker, `dim` all chrome — no other color is spent here, so
//! the card holds up on the `terminal` preset too.

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;
use wing_api_client::models::ModelDetail;

use crate::config::ThemePalette;
use crate::render::markdown::truncate_to_display_width;
use crate::shared::panels::PANEL_WINDOW;
use crate::shared::panels::SelectionPanel;
use crate::shared::panels::picker::ModelPanel;
use crate::shared::panels::window_range;

/// The card never grows past this.
const CARD_MAX_WIDTH: usize = 88;
/// Below this available width the card drops the detail line.
const FULL_MIN_WIDTH: usize = 64;
/// Below this available width the border is dropped for the rail form.
const RAIL_MAX_WIDTH: usize = 42;
/// Columns of padding inside the border, on each side.
const PAD: usize = 2;
/// The cursor column (`❯ ` / blank), so labels never shift.
const CURSOR_WIDTH: usize = 2;
/// Gap between two columns.
const GAP: usize = 2;
/// The label column's floor while a data column can still be dropped.
const LABEL_MIN_WIDTH: usize = 12;
/// The `vision` capability column.
const VISION_TEXT: &str = "vision";
const VISION_WIDTH: usize = 6;
/// The `● current` badge column, and its compact form.
const BADGE_WIDTH: usize = 9;
const BADGE_COMPACT_WIDTH: usize = 1;
/// The footer's position zone (`NN / NN`) — reserved even when blank, so the
/// card width does not jump when a page crosses the window size.
const POS_WIDTH: usize = 7;
/// The card's right margin inside the cell.
const RIGHT_MARGIN: usize = 2;

/// The empty page's one line of content — the width maths sizes the card for
/// it, so an explanation is never clipped.
const EMPTY_STATE: &str = "No models declared for this provider";

const HINT: &str = "↑↓ move · ←→ provider · Enter apply · Esc close";
const HINT_COMPACT: &str = "↑↓ move · ←→ provider · Enter · Esc";
const HINT_EMPTY: &str = "←→ provider · Esc close";
const HINT_MIN: &str = "↑↓ · Enter · Esc";

/// Render the picker for a cell `width` columns wide.
pub fn model_picker_lines(
    panel: &ModelPanel,
    width: u16,
    palette: &ThemePalette,
) -> Vec<Line<'static>> {
    let avail = width as usize;
    if avail == 0 {
        return vec![Line::from("")];
    }
    if avail < RAIL_MAX_WIDTH {
        return rail_lines(panel, avail, palette);
    }
    card_lines(panel, avail, palette)
}

// ── Data the render layer reads ─────────────────────────────────

/// Catalog metrics: the widest declared label and the widest `ctx` label, over
/// **every** provider. Stable per render, so tabs never resize the card.
fn catalog_metrics(panel: &ModelPanel) -> (usize, usize) {
    let mut max_label = 0;
    let mut max_ctx = 0;
    for group in panel.sources() {
        for detail in &group.models {
            max_label = max_label.max(detail.display_label().width());
            if let Some(tokens) = detail.context_window_tokens {
                max_ctx = max_ctx.max(ctx_label(tokens).width());
            }
        }
    }
    (max_label, max_ctx)
}

/// Compact context-window notation: `128k` / `500k` / `1M` / `1.5M`.
///
/// [`tokens`](crate::util::format::tokens) keeps one decimal (`500.0k`) — right
/// for a single reading, too noisy for a scannable column. Values landing on a
/// whole unit drop the fraction, the rest keep one digit.
fn ctx_label(tokens: u64) -> String {
    let fmt = |v: f64, suffix: char| -> String {
        if (v - v.round()).abs() < 0.05 {
            format!("{:.0}{suffix}", v.round())
        } else {
            format!("{v:.1}{suffix}")
        }
    };
    if tokens >= 1_000_000 {
        fmt(tokens as f64 / 1_000_000.0, 'M')
    } else if tokens >= 1_000 {
        fmt(tokens as f64 / 1_000.0, 'k')
    } else {
        tokens.to_string()
    }
}

/// Clip `text` to `max` columns, marking the cut with `…`.
fn clip(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    format!("{}…", truncate_to_display_width(text, max - 1))
}

/// `4 models` / `1 model`.
fn count_text(models: usize) -> String {
    format!("{models} model{}", if models == 1 { "" } else { "s" })
}

/// Declared context window of a row (`""` when undeclared).
fn ctx_text(detail: &ModelDetail) -> String {
    detail
        .context_window_tokens
        .map(ctx_label)
        .unwrap_or_default()
}

// ── Row builder ─────────────────────────────────────────────────

/// Column alignment of [`Row::cell`].
#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

/// One row under construction: styled spans plus the display width painted so
/// far. Every fixed column goes through [`Row::cell`], so alignment is
/// arithmetic — never string padding that a double-width glyph could break.
struct Row {
    spans: Vec<Span<'static>>,
    width: usize,
}

impl Row {
    fn new() -> Self {
        Self {
            spans: Vec::new(),
            width: 0,
        }
    }

    /// Push `text` verbatim under `style`.
    fn push(&mut self, text: impl Into<String>, style: Style) {
        let text = text.into();
        self.width += text.width();
        self.spans.push(Span::styled(text, style));
    }

    /// Push `text` inside a fixed-width column.
    fn cell(&mut self, text: &str, style: Style, width: usize, align: Align) {
        let body = clip(text, width);
        let fill = width - body.width();
        match align {
            Align::Left => {
                self.push(body, style);
                self.push(" ".repeat(fill), Style::default());
            }
            Align::Right => {
                self.push(" ".repeat(fill), Style::default());
                self.push(body, style);
            }
        }
    }

    /// Pad the row to exactly `width` columns.
    fn fill(&mut self, width: usize) {
        if width > self.width {
            self.push(" ".repeat(width - self.width), Style::default());
        }
    }

    /// Close a bordered row at exactly `inner` columns. `band` paints the
    /// selection background across the whole row — padding columns included,
    /// since a half-lit row reads as a rendering bug.
    fn finish(mut self, inner: usize, band: bool, palette: &ThemePalette) -> Line<'static> {
        let border = Style::default().fg(palette.dim);
        let mut spans: Vec<Span<'static>> = if band {
            let bg = Style::default().bg(palette.surface);
            let fill = inner.saturating_sub(self.width);
            let mut painted: Vec<Span<'static>> = std::mem::take(&mut self.spans)
                .into_iter()
                .map(|span| Span::styled(span.content.into_owned(), span.style.patch(bg)))
                .collect();
            if fill > 0 {
                painted.push(Span::styled(" ".repeat(fill), bg));
            }
            painted
        } else {
            self.fill(inner);
            self.spans
        };
        spans.insert(0, Span::styled("│".to_string(), border));
        spans.push(Span::styled("│".to_string(), border));
        Line::from(spans)
    }

    /// Close a borderless row (rail form) at exactly `width` columns.
    fn finish_plain(mut self, width: usize) -> Line<'static> {
        self.fill(width);
        Line::from(self.spans)
    }
}

// ── Card ────────────────────────────────────────────────────────

/// Which data columns the card shows, and the label column they leave.
struct Columns {
    ctx: usize,
    vision: bool,
    label: usize,
}

impl Columns {
    /// Columns between the cursor and the badge (everything but the label).
    fn fixed(&self, badge: usize) -> usize {
        let ctx = if self.ctx > 0 { GAP + self.ctx } else { 0 };
        let vision = if self.vision { GAP + VISION_WIDTH } else { 0 };
        CURSOR_WIDTH + ctx + vision + GAP + badge
    }
}

fn card_lines(panel: &ModelPanel, avail: usize, palette: &ThemePalette) -> Vec<Line<'static>> {
    let full = avail >= FULL_MIN_WIDTH;
    let models = panel.models();
    let hint = if models.is_empty() {
        HINT_EMPTY
    } else if full {
        HINT
    } else {
        HINT_COMPACT
    };
    let badge = if full {
        BADGE_WIDTH
    } else {
        BADGE_COMPACT_WIDTH
    };
    let (plan, width) = column_plan(panel, avail, badge);
    let inner = width - 2;

    let mut lines = Vec::new();
    lines.push(title_line(panel, width, palette));
    lines.push(blank_line(inner, palette));
    lines.extend(tab_rows(panel, inner, palette));
    lines.push(blank_line(inner, palette));
    lines.extend(row_lines(panel, inner, &plan, badge, full, palette));
    lines.push(blank_line(inner, palette));
    if full {
        lines.push(detail_line(panel, inner, palette));
    }
    lines.push(footer_line(panel, inner, hint, palette));
    lines.push(bottom_line(inner, palette));
    lines.push(Line::from(""));
    lines
}

/// Card width and column plan: the widest of what the rows, the tab strip and
/// the footer need, capped by the card limit and the cell; data columns are
/// dropped (`vision` first, then ctx) until the label column can breathe.
fn column_plan(panel: &ModelPanel, avail: usize, badge: usize) -> (Columns, usize) {
    let (max_label, max_ctx) = catalog_metrics(panel);
    let models = panel.models();
    let hint = if models.is_empty() {
        HINT_EMPTY
    } else if avail >= FULL_MIN_WIDTH {
        HINT
    } else {
        HINT_COMPACT
    };
    let tabs_need = PAD
        + tab_segments(panel, usize::MAX).plain_width()
        + GAP
        + count_text(models.len()).width()
        + PAD;
    let footer_need = PAD + hint.width() + GAP + POS_WIDTH + PAD;
    let rows_need = |plan: &Columns| {
        if models.is_empty() {
            PAD + EMPTY_STATE.width() + PAD
        } else {
            PAD + plan.fixed(badge) + plan.label + PAD
        }
    };
    let has_vision = catalog_has_vision(panel);
    let cap = avail.saturating_sub(RIGHT_MARGIN).min(CARD_MAX_WIDTH);

    let plan_of = |ctx: usize, vision: bool| Columns {
        ctx,
        vision: vision && has_vision,
        label: max_label,
    };
    let fit = |plan: Columns| {
        // `need` is measured inside the borders; the card adds its own two.
        let need = rows_need(&plan).max(tabs_need).max(footer_need) + 2;
        let width = cap.min(need);
        let label = (width - 2 - 2 * PAD).saturating_sub(plan.fixed(badge));
        (plan, width, label)
    };
    for (ctx, vision) in [(max_ctx, true), (max_ctx, false), (0, false)] {
        let (plan, width, label) = fit(plan_of(ctx, vision));
        if label >= LABEL_MIN_WIDTH {
            return (Columns { label, ..plan }, width);
        }
    }
    // Last resort: no data columns at all, labels clip to whatever is left.
    let (plan, width, label) = fit(Columns {
        ctx: 0,
        vision: false,
        label: max_label,
    });
    (
        Columns {
            label: label.max(1),
            ..plan
        },
        width,
    )
}

/// Whether any model in the catalog declares the vision capability.
fn catalog_has_vision(panel: &ModelPanel) -> bool {
    panel
        .sources()
        .iter()
        .flat_map(|group| &group.models)
        .any(|detail| detail.capabilities.vision)
}

fn blank_line(inner: usize, palette: &ThemePalette) -> Line<'static> {
    Row::new().finish(inner, false, palette)
}

/// `╭─ Model ──────…────── ● <label> · <provider> ─╮`
///
/// The status segment gives the rows' `●` its meaning and answers "what am I
/// on?" before the user reads a single row; it is dropped (leaving a plain
/// border) when the card is too narrow to carry it.
fn title_line(panel: &ModelPanel, width: usize, palette: &ThemePalette) -> Line<'static> {
    let dim = Style::default().fg(palette.dim);
    let text = Style::default()
        .fg(palette.text)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(palette.text);
    let success = Style::default().fg(palette.success);
    let head = " Model ";
    let mark = panel.current_mark();

    // `with_provider` marks the full status segment; the short one drops the
    // provider (the active tab still shows it).
    //
    // The fixed prefix is `╭` + `─` + ` Model `, and the closing `╮` is
    // another column; a status segment only goes in while 3 dashes survive.
    let prefix = head.width() + 2;
    const MIN_DASHES: usize = 3;
    let mut chosen: Option<(String, bool)> = None;
    if let Some((label, provider)) = mark {
        for candidate in [
            (format!(" ● {label} · {provider} ─"), true),
            (format!(" ● {label} ─"), false),
        ] {
            if prefix + MIN_DASHES + candidate.0.width() < width {
                chosen = Some(candidate);
                break;
            }
        }
    }
    let suffix_width = chosen.as_ref().map_or(1, |(s, _)| s.width());
    let dashes = width.saturating_sub(prefix + suffix_width + 1);

    let mut spans = vec![
        Span::styled("╭".to_string(), dim),
        Span::styled("─".to_string(), dim),
        Span::styled(head.to_string(), text),
        Span::styled("─".repeat(dashes), dim),
    ];
    match (chosen, mark) {
        (Some((_, with_provider)), Some((label, provider))) => {
            spans.push(Span::styled(" ".to_string(), dim));
            spans.push(Span::styled("●".to_string(), success));
            spans.push(Span::styled(" ".to_string(), dim));
            spans.push(Span::styled(label.to_string(), label_style));
            spans.push(Span::styled(
                if with_provider {
                    format!(" · {provider} ")
                } else {
                    " ".to_string()
                },
                dim,
            ));
            spans.push(Span::styled("─".to_string(), dim));
        }
        _ => spans.push(Span::styled("─".to_string(), dim)),
    }
    spans.push(Span::styled("╮".to_string(), dim));
    Line::from(spans)
}

fn bottom_line(inner: usize, palette: &ThemePalette) -> Line<'static> {
    let dim = Style::default().fg(palette.dim);
    Line::from(vec![
        Span::styled("╰".to_string(), dim),
        Span::styled("─".repeat(inner), dim),
        Span::styled("╯".to_string(), dim),
    ])
}

/// One visible tab of the strip.
struct Tab {
    text: String,
    active: bool,
}

/// The provider strip (windowed, clipped to `budget`) plus the active label's
/// column range inside it. Offsets are in **plain display columns**, so the
/// styled row and the underline row are two projections of one walk.
struct Strip {
    tabs: Vec<Tab>,
    active_start: usize,
    active_width: usize,
}

impl Strip {
    /// Columns painted between two tabs.
    const SEP: usize = 4;

    /// Plain width of the strip (separators included) — the width maths reads
    /// this through [`tab_segments`] before any style exists.
    fn plain_width(&self) -> usize {
        self.tabs.iter().map(|t| t.text.width()).sum::<usize>()
            + Self::SEP * self.tabs.len().saturating_sub(1)
    }
}

/// Walk the visible providers once, keeping whatever fits `budget` and
/// remembering where the active label sits. `‹` / `›` appear only when the
/// strip actually scrolls (an indicator that is always there tells nothing);
/// the trailing one is reserved for up front, so a squeezed strip still shows
/// that more providers wait to the right.
fn tab_segments(panel: &ModelPanel, budget: usize) -> Strip {
    let sources = panel.sources();
    let range = window_range(panel.current_page(), sources.len(), PANEL_WINDOW);
    let current = panel.current_page();

    let mut candidates: Vec<(String, bool)> = Vec::new();
    if range.start > 0 {
        candidates.push(("‹".to_string(), false));
    }
    for (i, group) in sources.iter().enumerate().take(range.end).skip(range.start) {
        candidates.push((group.provider.clone(), i == current));
    }
    let tail_marker = range.end < sources.len();

    let reserve = if tail_marker { Strip::SEP + 1 } else { 0 };
    let mut tabs: Vec<Tab> = Vec::new();
    let mut width = 0usize;
    let mut active_start = 0;
    let mut active_width = 0;
    for (text, active) in candidates {
        let gap = if tabs.is_empty() { 0 } else { Strip::SEP };
        if width + gap + text.width() + reserve > budget {
            break;
        }
        if active {
            active_start = width + gap;
            active_width = text.width();
        }
        width += gap + text.width();
        tabs.push(Tab { text, active });
    }
    if tail_marker {
        let gap = if tabs.is_empty() { 0 } else { Strip::SEP };
        if width + gap < budget {
            tabs.push(Tab {
                text: "›".to_string(),
                active: false,
            });
        }
    }
    Strip {
        tabs,
        active_start,
        active_width,
    }
}

/// Tab strip + the `▔` underline row.
fn tab_rows(panel: &ModelPanel, inner: usize, palette: &ThemePalette) -> Vec<Line<'static>> {
    let dim = Style::default().fg(palette.dim);
    let accent = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);
    let strip = tab_segments(panel, inner - 2 * PAD);

    let mut tabs = Row::new();
    tabs.push(" ".repeat(PAD), Style::default());
    for (i, tab) in strip.tabs.iter().enumerate() {
        if i > 0 {
            tabs.push(" ".repeat(Strip::SEP), Style::default());
        }
        tabs.push(tab.text.clone(), if tab.active { accent } else { dim });
    }
    let count = count_text(panel.models().len());
    let count_width = count.width();
    if tabs.width + GAP + count_width + PAD <= inner {
        tabs.fill(inner - PAD - count_width);
        tabs.push(count, dim);
    }

    let mut underline = Row::new();
    underline.push(" ".repeat(PAD + strip.active_start), Style::default());
    if strip.active_width > 0 {
        underline.push(
            "▔".repeat(strip.active_width),
            Style::default().fg(palette.accent),
        );
    }
    vec![
        tabs.finish(inner, false, palette),
        underline.finish(inner, false, palette),
    ]
}

/// The model rows: label, data columns, the current badge; the cursor row
/// carries the selection band.
fn row_lines(
    panel: &ModelPanel,
    inner: usize,
    plan: &Columns,
    badge: usize,
    full: bool,
    palette: &ThemePalette,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(palette.dim);
    let text = Style::default().fg(palette.text);
    let models = panel.models();

    if models.is_empty() {
        let mut row = Row::new();
        row.push(" ".repeat(PAD), Style::default());
        row.push(clip(EMPTY_STATE, inner - 2 * PAD), dim);
        return vec![row.finish(inner, false, palette)];
    }

    let committed = panel.committed_at(panel.current_page());
    let cursor = panel.cursor();
    let mut lines = Vec::new();
    for index in window_range(cursor, models.len(), PANEL_WINDOW) {
        let detail = &models[index];
        let is_cursor = index == cursor;
        let mut row = Row::new();
        row.push(" ".repeat(PAD), Style::default());
        if is_cursor {
            row.push(
                "❯ ",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            );
        } else {
            row.push(" ".repeat(CURSOR_WIDTH), Style::default());
        }
        row.cell(
            detail.display_label(),
            if is_cursor {
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                text
            },
            plan.label,
            Align::Left,
        );
        // The columns are independent: a catalog may declare vision on models
        // that never declare a window.
        if plan.ctx > 0 {
            row.push(" ".repeat(GAP), Style::default());
            row.cell(&ctx_text(detail), dim, plan.ctx, Align::Right);
        }
        if plan.vision {
            row.push(" ".repeat(GAP), Style::default());
            row.cell(
                if detail.capabilities.vision {
                    VISION_TEXT
                } else {
                    ""
                },
                dim,
                VISION_WIDTH,
                Align::Left,
            );
        }
        row.push(" ".repeat(GAP), Style::default());
        if committed == Some(index) {
            row.push("●".to_string(), Style::default().fg(palette.success));
            if full {
                row.push(" current".to_string(), dim);
            }
        } else {
            row.push(" ".repeat(badge), Style::default());
        }
        lines.push(row.finish(inner, is_cursor, palette));
    }
    lines
}

/// The live detail line: `id …` (when it differs from the label) `· keep …`
/// `· <description>` — blank when nothing is declared.
fn detail_line(panel: &ModelPanel, inner: usize, palette: &ThemePalette) -> Line<'static> {
    let dim = Style::default().fg(palette.dim);
    let mut parts: Vec<String> = Vec::new();
    if let Some(detail) = panel.model_at(panel.cursor()) {
        if detail.id != detail.display_label() {
            parts.push(format!("id {}", detail.id));
        }
        if let Some(keep) = detail.keep_recent_tokens {
            parts.push(format!("keep {}", ctx_label(keep)));
        }
        if let Some(description) = detail
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            parts.push(description.to_string());
        }
    }
    let mut row = Row::new();
    row.push(" ".repeat(PAD), Style::default());
    row.push(clip(&parts.join(" · "), inner - 2 * PAD), dim);
    row.finish(inner, false, palette)
}

/// Keys on the left, position on the right.
fn footer_line(
    panel: &ModelPanel,
    inner: usize,
    hint: &str,
    palette: &ThemePalette,
) -> Line<'static> {
    let dim = Style::default().fg(palette.dim);
    let models = panel.models();
    let hint = if PAD + hint.width() + GAP + POS_WIDTH + PAD > inner {
        HINT_MIN
    } else {
        hint
    };
    let pos = if models.is_empty() {
        String::new()
    } else {
        format!("{} / {}", panel.cursor() + 1, models.len())
    };
    let pos = if PAD + hint.width() + GAP + pos.width() + PAD <= inner {
        pos
    } else {
        String::new()
    };
    let mut row = Row::new();
    row.push(" ".repeat(PAD), Style::default());
    row.push(hint.to_string(), dim);
    row.fill(inner - PAD - pos.width());
    if !pos.is_empty() {
        row.push(pos, dim);
    }
    row.finish(inner, false, palette)
}

// ── Rail (too narrow for a border) ──────────────────────────────

/// Borderless degrade: title line, list, one hint line. No columns, no detail
/// line — the cursor, the badge and the labels are the same as the card's.
fn rail_lines(panel: &ModelPanel, avail: usize, palette: &ThemePalette) -> Vec<Line<'static>> {
    let dim = Style::default().fg(palette.dim);
    let accent = Style::default()
        .fg(palette.accent)
        .add_modifier(Modifier::BOLD);
    let text = Style::default().fg(palette.text);
    let success = Style::default().fg(palette.success);
    let models = panel.models();
    let mut lines = Vec::new();

    let mut head = Row::new();
    let title = format!("Model · {}", panel.provider());
    let title = if title.width() + GAP + count_text(models.len()).width() <= avail {
        title
    } else {
        panel.provider().to_string()
    };
    head.push(title, text.add_modifier(Modifier::BOLD));
    let count = count_text(models.len());
    if head.width + GAP + count.width() <= avail {
        head.fill(avail - count.width());
        head.push(count, dim);
    }
    lines.push(head.finish_plain(avail));

    if models.is_empty() {
        lines.push(Line::from(Span::styled(
            clip("No models declared", avail),
            dim,
        )));
    }
    let committed = panel.committed_at(panel.current_page());
    let cursor = panel.cursor();
    for index in window_range(cursor, models.len(), PANEL_WINDOW) {
        let detail = &models[index];
        let is_cursor = index == cursor;
        let mut row = Row::new();
        if is_cursor {
            row.push("❯ ", accent);
        } else {
            row.push(" ".repeat(CURSOR_WIDTH), Style::default());
        }
        let reserved = CURSOR_WIDTH + if committed == Some(index) { GAP } else { 0 };
        row.push(
            clip(detail.display_label(), avail.saturating_sub(reserved)),
            if is_cursor { accent } else { text },
        );
        if committed == Some(index) {
            row.push(" ".to_string(), Style::default());
            row.push("●".to_string(), success);
        }
        lines.push(row.finish_plain(avail));
    }
    lines.push(Line::from(Span::styled(clip(HINT_MIN, avail), dim)));
    lines.push(Line::from(""));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use wing_api_client::models::ModelCapabilities;
    use wing_api_client::models::ProviderModels;

    fn palette() -> ThemePalette {
        ThemePalette::default()
    }

    /// One model declaration; `None` fields stay undeclared (the wire's
    /// optional columns).
    fn model(
        name: &str,
        display: Option<&str>,
        ctx: Option<u64>,
        keep: Option<u64>,
        vision: bool,
    ) -> ModelDetail {
        ModelDetail {
            id: name.to_string(),
            name: name.to_string(),
            display_name: display.map(str::to_string),
            description: None,
            context_window_tokens: ctx,
            keep_recent_tokens: keep,
            capabilities: ModelCapabilities { vision },
        }
    }

    /// The catalog the design work used: three providers, one page with the
    /// full set of declarations, one with none, one with long call names.
    fn hero_sources() -> Vec<ProviderModels> {
        vec![
            ProviderModels {
                provider: "dashscope".into(),
                models: vec![
                    model("qwen3.8-max", None, None, None, false),
                    model("deepseek-v4.1-flash", None, None, None, false),
                ],
            },
            ProviderModels {
                provider: "MacMachine".into(),
                models: vec![
                    model(
                        "dfmodel",
                        Some("DeepSeek-Flash"),
                        Some(500_000),
                        Some(100_000),
                        true,
                    ),
                    model(
                        "gmodel",
                        Some("GLM-5.3"),
                        Some(500_000),
                        Some(100_000),
                        true,
                    ),
                    model(
                        "qmodel_38max",
                        Some("Qwen3.8-max"),
                        Some(500_000),
                        Some(100_000),
                        true,
                    ),
                    model(
                        "qfmodel",
                        Some("Qwen3.8-Flash"),
                        Some(500_000),
                        Some(100_000),
                        true,
                    ),
                ],
            },
            ProviderModels {
                provider: "magpie".into(),
                models: vec![
                    model(
                        "workbuddy-ai/deepseek-v4.1-flash",
                        None,
                        Some(500_000),
                        Some(100_000),
                        true,
                    ),
                    model("qoder/qfmodel", None, Some(500_000), Some(100_000), true),
                ],
            },
        ]
    }

    fn hero_panel() -> ModelPanel {
        ModelPanel::new(hero_sources(), Some("dfmodel"))
    }

    fn render(panel: &ModelPanel, width: u16) -> String {
        model_picker_lines(panel, width, &palette())
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Display width of every rendered line, borders included.
    fn line_widths(panel: &ModelPanel, width: u16) -> Vec<usize> {
        model_picker_lines(panel, width, &palette())
            .iter()
            .map(|line| line.to_string().width())
            .collect()
    }

    /// Display column of `needle` inside `line` (byte offsets are wrong once
    /// box-drawing glyphs precede it).
    fn column_of(line: &str, needle: &str) -> usize {
        let byte = line.find(needle).expect("needle in line");
        line[..byte].width()
    }

    fn down(panel: &mut ModelPanel, times: usize) {
        for _ in 0..times {
            panel.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Down,
                crossterm::event::KeyModifiers::NONE,
            ));
        }
    }

    fn right(panel: &mut ModelPanel, times: usize) {
        for _ in 0..times {
            panel.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Right,
                crossterm::event::KeyModifiers::NONE,
            ));
        }
    }

    // ── The card ────────────────────────────────────────────────

    /// The design's hero frame, column-exact: title bar with the active
    /// model, underlined tabs, data columns, badge, detail line, footer.
    #[test]
    fn full_card_matches_the_design() {
        let out = render(&hero_panel(), 100);
        let expected = "\
╭─ Model ─────────────────────── ● DeepSeek-Flash · MacMachine ─╮
│                                                               │
│  dashscope    MacMachine    magpie                  4 models  │
│               ▔▔▔▔▔▔▔▔▔▔                                      │
│                                                               │
│  ❯ DeepSeek-Flash                    500k  vision  ● current  │
│    GLM-5.3                           500k  vision             │
│    Qwen3.8-max                       500k  vision             │
│    Qwen3.8-Flash                     500k  vision             │
│                                                               │
│  id dfmodel · keep 100k                                       │
│  ↑↓ move · ←→ provider · Enter apply · Esc close       1 / 4  │
╰───────────────────────────────────────────────────────────────╯
";
        assert_eq!(out, expected);
    }

    /// Moving the cursor moves the band and the detail line only; the `●
    /// current` badge stays on the session's model.
    #[test]
    fn cursor_moves_the_detail_line_not_the_mark() {
        let mut panel = hero_panel();
        down(&mut panel, 2);
        let out = render(&panel, 100);
        assert!(out.contains("❯ Qwen3.8-max"), "{out}");
        assert!(
            out.contains("    DeepSeek-Flash                    500k  vision  ● current"),
            "{out}"
        );
        assert!(
            out.contains("id qmodel_38max · keep 100k"),
            "detail follows the cursor: {out}"
        );
        assert!(out.contains("3 / 4"), "{out}");
    }

    /// Columns are catalog-wide: every provider page draws the same card, so
    /// switching tabs never moves the eye's landmarks.
    #[test]
    fn card_width_is_stable_across_providers() {
        let mut panel = hero_panel();
        let mut widths = Vec::new();
        for _ in 0..3 {
            let lines = line_widths(&panel, 100);
            let card = *lines.first().expect("title line");
            assert!(
                lines.iter().all(|w| *w == card || *w == 0),
                "every line of a card shares one width: {lines:?}"
            );
            widths.push(card);
            right(&mut panel, 1);
        }
        assert_eq!(widths[0], widths[1], "MacMachine vs magpie: {widths:?}");
        assert_eq!(widths[0], widths[2], "MacMachine vs dashscope: {widths:?}");
    }

    /// Declared data renders in its column; undeclared cells stay blank (the
    /// column is catalog-wide, not row-local).
    #[test]
    fn data_columns_show_declarations_and_reserve_gaps() {
        let out = render(&hero_panel(), 100);
        assert!(out.contains("500k  vision"), "ctx + vision columns: {out}");
        let mut panel = hero_panel();
        panel.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Left,
            crossterm::event::KeyModifiers::NONE,
        )); // dashscope: nothing declared anywhere on that page
        let out = render(&panel, 100);
        assert!(out.contains("❯ qwen3.8-max"), "{out}");
        assert!(
            !out.contains("vision"),
            "no declared capability, no vision column content: {out}"
        );
        assert!(
            out.contains("❯ qwen3.8-max"),
            "the catalog-wide columns stay reserved: {out}"
        );
    }

    /// A catalog declaring vision but no context windows keeps the vision
    /// column: it is its own column, not a suffix of ctx.
    #[test]
    fn vision_column_survives_without_any_window_declaration() {
        let sources = vec![ProviderModels {
            provider: "p".into(),
            models: vec![
                model("sees", Some("Sees-Images"), None, None, true),
                model("blind", Some("Text-Only"), None, None, false),
            ],
        }];
        let out = render(&ModelPanel::new(sources, Some("sees")), 100);
        let row = |label: &str| {
            out.lines()
                .find(|l| l.contains(label))
                .unwrap_or_else(|| panic!("{label} row missing: {out}"))
        };
        // The cursor row ends with the reserved badge column; the text-only
        // row keeps the same columns (both lines are card-wide).
        assert!(
            row("❯ Sees-Images").ends_with("vision  ● current  │"),
            "{out}"
        );
        assert!(!row("Text-Only").contains("vision"), "{out}");
        assert!(!row("Text-Only").contains('●'), "{out}");
        let widths: Vec<usize> = out.lines().map(|l| l.width()).collect();
        assert!(
            widths.iter().all(|w| *w == widths[0] || *w == 0),
            "{widths:?}"
        );
    }

    /// The ctx notation drops a whole-unit fraction and keeps one digit
    /// otherwise: `1M` / `1.5M` / `131.1k` / `500k` / `999`.
    #[test]
    fn ctx_label_is_compact() {
        assert_eq!(ctx_label(999), "999");
        assert_eq!(ctx_label(1_000), "1k");
        assert_eq!(ctx_label(128_000), "128k");
        assert_eq!(ctx_label(131_072), "131.1k");
        assert_eq!(ctx_label(500_000), "500k");
        assert_eq!(ctx_label(1_000_000), "1M");
        assert_eq!(ctx_label(1_048_576), "1M");
        assert_eq!(ctx_label(1_500_000), "1.5M");
    }

    /// A label wider than its column clips with `…` and never pushes the data
    /// columns around.
    #[test]
    fn long_labels_clip_without_moving_the_columns() {
        let sources = vec![ProviderModels {
            provider: "magpie".into(),
            models: vec![model(
                "workbuddy-ai/a-very-long-model-name-indeed",
                None,
                Some(500_000),
                Some(100_000),
                true,
            )],
        }];
        let panel = ModelPanel::new(sources, None);
        let out = render(&panel, 60);
        assert!(out.contains("…"), "{out}");
        assert!(out.contains("500k"), "data column kept: {out}");
        assert!(!out.contains("a-very-long-model-name-indeed"), "{out}");
    }

    /// The live detail line reads the highlighted row: the declared id only
    /// when it differs from the label, plus the compaction reserve and the
    /// description when declared.
    #[test]
    fn detail_line_reads_id_keep_and_description() {
        let sources = vec![ProviderModels {
            provider: "p".into(),
            models: vec![
                ModelDetail {
                    description: Some("Fast and cheap".into()),
                    ..model(
                        "ds-flash",
                        Some("DeepSeek-Flash"),
                        Some(500_000),
                        Some(100_000),
                        true,
                    )
                },
                // id == label: no `id` prefix to repeat, keep undeclared.
                model("plain", None, None, None, false),
            ],
        }];
        let mut panel = ModelPanel::new(sources, None);
        let out = render(&panel, 100);
        assert!(
            out.contains("id ds-flash · keep 100k · Fast and cheap"),
            "{out}"
        );
        down(&mut panel, 1);
        let out = render(&panel, 100);
        assert!(
            !out.contains("id plain") && !out.contains("keep"),
            "nothing declared reads as a blank detail line: {out}"
        );
        assert!(out.contains("❯ plain"), "{out}");
    }

    /// A session without a resolved model id (legacy metadata / identify
    /// miss): no title status, no badge — the card never invents a model.
    #[test]
    fn unmarked_catalog_has_no_status_and_no_badge() {
        let panel = ModelPanel::new(hero_sources(), None);
        let out = render(&panel, 100);
        assert!(!out.contains("current"), "{out}");
        assert!(!out.contains('●'), "{out}");
        assert!(
            out.starts_with("╭─ Model ─"),
            "the title keeps its plain border: {out}"
        );
    }

    // ── Window / overflow ───────────────────────────────────────

    /// More than [`PANEL_WINDOW`] models: five rows around the cursor and the
    /// position counter.
    #[test]
    fn long_pages_window_five_rows_and_report_the_position() {
        let models: Vec<ModelDetail> = (0..8)
            .map(|i| model(&format!("m{i}"), None, None, None, false))
            .collect();
        let panel = {
            let mut panel = ModelPanel::new(
                vec![ProviderModels {
                    provider: "p".into(),
                    models,
                }],
                None,
            );
            down(&mut panel, 6);
            panel
        };
        let out = render(&panel, 100);
        assert!(out.contains("❯ m6"), "{out}");
        assert!(out.contains("7 / 8"), "{out}");
        for hidden in ["m0", "m1"] {
            assert!(!out.contains(hidden), "{hidden} above the window: {out}");
        }
        assert!(!out.contains("m8"), "{out}");
    }

    /// More than [`PANEL_WINDOW`] providers: the strip scrolls around the
    /// active tab and marks both edges, the underline follows it.
    #[test]
    fn long_provider_strips_mark_their_overflow() {
        let sources: Vec<ProviderModels> = (0..7)
            .map(|i| ProviderModels {
                provider: format!("p{i}"),
                models: vec![model("m", None, None, None, false)],
            })
            .collect();
        let mut panel = ModelPanel::new(sources, None);
        right(&mut panel, 5);
        let out = render(&panel, 100);
        assert!(out.contains("‹    p2    p3    p4    p5    p6"), "{out}");
        assert!(!out.contains("p0") && !out.contains("p1"), "{out}");
        assert!(!out.contains('›'), "p6 is the last provider: {out}");
        // The underline sits under the active label (p5), not under its slot.
        let tabs = out.lines().find(|l| l.contains("p2")).unwrap();
        let underline = out.lines().find(|l| l.contains('▔')).unwrap();
        assert_eq!(
            column_of(underline, "▔"),
            column_of(tabs, "p5"),
            "underline under the active tab: {out}"
        );
    }

    /// Even under pressure every line is exactly as wide as the card — a
    /// wrapped border would be a rendering bug, not a degrade. The unmarked
    /// catalog exercises the title's plain-border branch.
    #[test]
    fn every_line_fits_the_cell_at_every_width() {
        for width in [0u16, 12, 34, 40, 42, 48, 56, 63, 64, 80, 100, 160] {
            let unmarked = ModelPanel::new(hero_sources(), None);
            let unmarked_widths = line_widths(&unmarked, width);
            for (i, w) in unmarked_widths.iter().enumerate() {
                assert!(
                    *w <= width as usize,
                    "unmarked line {i} is {w} wide in a {width}-column cell"
                );
            }
            let widths = line_widths(&hero_panel(), width);
            let card = widths.first().copied().unwrap_or_default();
            let rail = width < RAIL_MAX_WIDTH as u16;
            for (i, w) in widths.iter().enumerate() {
                assert!(
                    *w <= width as usize,
                    "line {i} is {w} wide in a {width}-column cell: {widths:?}"
                );
                if *w > 0 && !rail {
                    assert_eq!(*w, card, "line {i} breaks the card's width: {widths:?}");
                }
            }
        }
    }

    // ── Degradation ladder ──────────────────────────────────────

    /// Compact tier: the detail line and the `current` word go, the data
    /// columns and the bare `●` stay.
    #[test]
    fn compact_tier_drops_the_detail_line() {
        let out = render(&hero_panel(), 56);
        assert!(out.contains("❯ DeepSeek-Flash"), "{out}");
        assert!(out.contains("●") && !out.contains("current"), "{out}");
        assert!(!out.contains("id dfmodel"), "{out}");
        assert!(out.contains("500k"), "columns survive: {out}");
        assert!(out.contains("1 / 4"), "{out}");
    }

    /// Rail tier: no border at all, but the list, the cursor and the badge
    /// stay legible.
    #[test]
    fn rail_tier_drops_the_border() {
        let out = render(&hero_panel(), 34);
        assert!(
            !out.contains('╭') && !out.contains('│') && !out.contains('╰'),
            "{out}"
        );
        assert!(out.starts_with("Model · MacMachine"), "{out}");
        assert!(out.contains("❯ DeepSeek-Flash ●"), "{out}");
        assert!(out.contains("↑↓ · Enter · Esc"), "{out}");
    }

    /// An empty provider page keeps the card's shape and explains itself
    /// instead of showing a bare `(no models)`.
    #[test]
    fn empty_provider_renders_the_empty_state() {
        let sources = vec![
            ProviderModels {
                provider: "full".into(),
                models: vec![model("m", None, None, None, false)],
            },
            ProviderModels {
                provider: "empty".into(),
                models: vec![],
            },
        ];
        let mut panel = ModelPanel::new(sources, None);
        right(&mut panel, 1);
        let out = render(&panel, 100);
        assert!(
            out.contains("No models declared for this provider"),
            "{out}"
        );
        assert!(out.contains("0 models"), "{out}");
        assert!(
            !out.contains(" / "),
            "no cursor position without rows: {out}"
        );
    }

    /// A catalog with nothing in it still renders a card (the app refuses to
    /// open the panel with an empty fetch, but the cell never panics).
    #[test]
    fn empty_catalog_renders_an_empty_card() {
        let panel = ModelPanel::new(vec![], None);
        let out = render(&panel, 100);
        assert!(
            out.contains("No models declared for this provider"),
            "{out}"
        );
        assert!(out.contains("0 models"), "{out}");
        assert!(!out.contains('●'), "{out}");
    }

    /// The count reads naturally for a single model.
    #[test]
    fn single_model_page_uses_the_singular() {
        let sources = vec![ProviderModels {
            provider: "p".into(),
            models: vec![model("only", None, None, None, false)],
        }];
        let out = render(&ModelPanel::new(sources, None), 100);
        assert!(
            out.contains("1 model ") || out.contains("1 model│"),
            "{out}"
        );
        assert!(!out.contains("1 models"), "{out}");
    }
}
