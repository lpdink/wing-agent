//! Status bar widget — top row showing model, tokens, usage, session identity.
//!
//! ## Registers — the left chain reads as a sentence
//!
//! `Wing · <session id> ★ · ▮<model>▮ · think:<effort> · <provider>`, then the
//! instruments flush right. Each segment owns exactly one register:
//!
//! | segment | what it says | ink |
//! |---|---|---|
//! | brand | "me" | `text` — the row's only bold |
//! | identity | "this session" | `dim` (`warning` once pinned) |
//! | model | "who I'm talking to" | [`Chip::key`] — a `surface` band, not a brighter gray |
//! | mode | "how it thinks" | `accent` — the row's only accent |
//! | provider | "where the model comes from" | `dim` |
//!
//! The model is the only segment that degrades to fit (padded block → bare
//! block; every width comes from `ui::chip`), so the row keeps its model down
//! to the width where even the bare label no longer fits. Everything else is
//! all-or-nothing: a half-drawn block reads worse than no block. The mode
//! sits *before* the provider because an accent that truncation can eat is no
//! longer an accent — the provider is the most ambient fact here (`wing info`
//! and the `/model` picker carry the whole byline).
//!
//! The session id and the pin star are separate segments so the copyable id
//! outlives the star on a very narrow row — the id is the only visible handle
//! for "click to copy". The star never stands alone: a tail without an
//! identity says nothing.
//!
//! A segment stays put while the ones after it come and go; the one exception
//! is the model's own rung, which follows the room the right cluster leaves
//! (a wider read-out can cost the block its padding, and then the segments
//! after it shift two columns). Nothing else in the chain moves on its own.
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
use crate::ui::chip::Chip;
use crate::ui::chip::Pad;
use crate::util::format;

/// 模型身份四元组——一个会话表面上「当前模型」的完整陈述。
///
/// 四个字段是**同一个时钟的同一份状态**（info / 事件 / 快照 / 乐观更新都给全），
/// 因此以一组整体消费：model 值变化时它们一起换，不存在「新模型配旧 provider」的
/// 中间态。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelUpdate {
    /// 引用词（∈ 配置声明的 id 空间）；不可用时 None（旧会话 / 反查不中）。
    /// 前端的选择态 / 匹配以它为准——但绝不发明它。
    pub id: Option<String>,
    /// 实际调用名（发给上游的值；展示回落素材）。
    pub name: String,
    /// 活跃 provider 名（运行期事实；降级路径可能缺席）。
    pub provider: Option<String>,
    /// 配置声明的展示名（未声明 / 空串 = None，展示回落 `name`）。
    pub display_name: Option<String>,
}

/// Application state for the status bar.
#[derive(Debug, Clone)]
pub struct StatusData {
    /// Active model's call name (display fallback — never the identity).
    pub model: String,
    /// Active model's reference word (`model_id`); None when the gateway could
    /// not resolve one (old session metadata / identify miss). Display-only
    /// consumers ignore it; the picker shows no mark without it.
    pub model_id: Option<String>,
    /// Display label declared for `model` by the gateway config; `None` when
    /// undeclared / on old gateways. Display-only — identity is `model_id`.
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
            model_id: None,
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
    /// the same defense the `/model` picker applies (`display_label`), so a
    /// whitespace-only label can never blank out the status bar.
    pub fn model_label(&self) -> &str {
        self.model_display_name
            .as_deref()
            .filter(|label| !label.trim().is_empty())
            .unwrap_or(&self.model)
    }

    /// Replace the whole model identity group (the four fields travel together).
    pub fn set_model(&mut self, model: ModelUpdate) {
        self.model = model.name;
        self.model_id = model.id;
        self.provider = model.provider;
        self.model_display_name = model.display_name;
    }

    /// Apply optional session-state fields from a server event or optimistic update.
    ///
    /// The model identity group (`model` / `model_id` / `provider` /
    /// `model_display_name`) is consumed — and dropped — together: they
    /// describe the same instant of the same model, so a `None` model means
    /// "the model is unchanged" while a present one replaces all four (a stale
    /// label must never describe the new model). A model without an id keeps
    /// `model_id = None` (old gateway / identify miss) — the frontend tolerates
    /// it but never invents one.
    ///
    /// Parameter order mirrors the remaining session-state fields of
    /// `AppIntent::UpdateSession` so that callers destructuring the variant can
    /// pass fields through positionally.
    #[allow(clippy::too_many_arguments)] // flat mirror of the session-state fields
    pub fn apply_session_update(
        &mut self,
        model: Option<ModelUpdate>,
        agent: Option<String>,
        title: Option<String>,
        thinking: Option<bool>,
        reasoning_effort: Option<String>,
        yolo: Option<bool>,
    ) {
        if let Some(m) = model {
            self.set_model(m);
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

/// 左链上的一段：分隔符与内容一起进出。
///
/// 截断发生在段边界（放不下的段整段不画），所以不会留下「只有一个中间点、后面
/// 没有东西」的悬空分隔符 —— 那比少一条事实更难看。段内的 span 各自带样式，
/// 有的还声明命中区（见 [`Slot`]）。
///
/// 同类规则的另一个实现是输入卡的 `rail`（`ui::input_area::chrome`）：右边读数先
/// 预留、左边条目整条丢。两处都不必抽公共引擎（那边是带角字符的边框行，这边要
/// 回填命中区并逐段挑档位），但别再长出第三份。
struct Segment(Vec<(Span<'static>, Option<Slot>)>);

impl Segment {
    /// 这一段占多少列（宽度只看 span，不看样式）。
    fn width(&self) -> u16 {
        self.0.iter().map(|(span, _)| span.width() as u16).sum()
    }
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
    /// 宽终端开关（`app::WIDE_THRESHOLD`）：完整会话 ID / 累计用量那一栏 /
    /// 模型段画不画底，三处都读它。
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

    /// 左链：品牌 → 会话身份 → 模型 → 状态（模式 / provider），按重要性排队。
    ///
    /// `room` 是右端仪表让出后剩下的列数（右端优先，见模块文档）。**只有模型段**
    /// 会为了塞进去降级（见 [`Self::model_segment`]），其余段整段在或整段不在 ——
    /// 截断在段边界发生，画一半的色块比没有更难读。
    ///
    /// 两个命中区（id / 星标）在绘制时回填，本函数只声明哪几段认领指针；`Slot`
    /// 出现的先后就是屏幕上的先后。
    fn left_chain(&self, room: u16) -> Vec<Segment> {
        let d = self.data;
        let dim = Style::default().fg(self.palette.dim);
        let mut segments: Vec<Segment> = Vec::new();

        // 品牌 =「我」：全行唯一加粗的一处。
        segments.push(Segment(vec![(
            Span::styled(
                " Wing",
                Style::default()
                    .fg(self.palette.text)
                    .add_modifier(Modifier::BOLD),
            ),
            None,
        )]));

        // 身份 =「这次会话」：引用材料，安静的一档。
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
            // ID 与星标各成一段：星标是 unpin 的唯一入口，但 ID 是复制目标的唯一
            // 可见载体 —— 窄到差一两列时先让星标（它自己的分隔符是一格空格，让出
            // 之后不会留下悬空的 `·`）。
            segments.push(Segment(vec![
                (Span::styled(" · ", dim), None),
                (Span::styled(id_text, dim), Some(Slot::SessionId)),
            ]));
            segments.push(Segment(vec![
                (Span::styled(" ", dim), None),
                (Span::styled(star_glyph, star_style), Some(Slot::Star)),
            ]));
        }

        // 模型 =「正在跟谁说话」：一块安静的底（`Chip::key`），先量前面已经用掉
        // 多少列，再决定这一块要哪一档。星标那 2 列无条件计入 —— 它让出时链条已经
        // 在那里中断（后面的段一个都不画），所以不存在"回收"这件事。
        let used: u16 = segments.iter().map(Segment::width).sum();
        segments.push(self.model_segment(room.saturating_sub(used)));

        // 模式 = 全行唯一的强调点（`accent` 墨）。文字与拼接规则不变：`think` /
        // `think:high`。
        if d.thinking {
            let label = match &d.reasoning_effort {
                Some(effort) => format!("think:{effort}"),
                None => "think".to_string(),
            };
            segments.push(Segment(vec![
                (Span::styled(" · ", dim), None),
                (
                    Span::styled(label, Style::default().fg(self.palette.accent)),
                    None,
                ),
            ]));
        }

        // Provider 在模式之后：它是这条链上最"环境"的一条事实 —— 模式是被强调的
        // 状态（被截断吃掉的强调点就不成其为强调点），provider 是这模型从哪来
        // （`wing info` 与 `/model` 面板里都有整份），所以先让出。
        if let Some(provider) = d.provider.as_deref().filter(|p| !p.is_empty()) {
            segments.push(Segment(vec![
                (Span::styled(" · ", dim), None),
                (Span::styled(provider.to_string(), dim), None),
            ]));
        }

        segments
    }

    /// 模型段：` · ` 与模型内容一起进出，先扣掉分隔符再挑块的档位。
    ///
    /// 宽度、底色、墨全在 `ui::chip` 里，这里只挑第一个塞得进 `room` 的档：
    ///
    /// 1. 有一格气口的块（`Pad::One`）：宽终端上的常规形态；
    /// 2. 没有气口的块（`Pad::Zero`）：行紧了先让气口，字一个不少。
    ///
    /// 块一档都放不下时就是**纯文本**（`wide == false` 的形态，墨是 `text`、
    /// 不参与块的宽度数学）。无气口的块与它同宽，所以宽终端上这一档只是几何等价：
    /// 塞不下时绘制循环整段不画 —— 模型宁可缺席，也不切字。
    ///
    /// 只量自己的宽度、不改别段的内容；挑到哪一档会改变后面几段的起点（见模块文档）。
    fn model_segment(&self, room: u16) -> Segment {
        let sep = Span::styled(" · ", Style::default().fg(self.palette.dim));
        let room = room.saturating_sub(sep.width() as u16);
        let label = self.data.model_label();
        let plain = || Span::styled(label.to_string(), Style::default().fg(self.palette.text));

        let mut spans: Vec<Span<'static>> = Vec::new();
        if self.wide {
            let chip = Chip::key(label);
            for pad in Pad::LADDER {
                if chip.width(pad) as u16 <= room {
                    spans.push(chip.span(self.palette, pad));
                    break;
                }
            }
        }
        if spans.is_empty() {
            spans.push(plain());
        }

        let mut segment = Segment(vec![(sep, None)]);
        segment.0.extend(spans.into_iter().map(|span| (span, None)));
        segment
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
            // 空的轨道用细横线而不是 `░`：25% 阴影块在暗底上读起来是"满格的灰条"，
            // 空会话的 `0/500.0k` 因此恰好长得像用完了（截图里就是这么骗人的）。
            right_spans.push(Span::styled("─".repeat(empty), dim));
            // 两侧同一套记法：`100k / 200k`。混用（`100.0k / 200k`）会让读数看起来
            // 像两个不同精度的仪表拼在一起。
            right_spans.push(Span::styled(
                format!(
                    " {} / {}",
                    format::tokens_capacity(d.total_tokens.max(0) as u64),
                    format::tokens_capacity(d.context_window_tokens.max(0) as u64),
                ),
                dim,
            ));
        } else {
            right_spans.push(Span::styled(
                format::tokens(d.total_tokens),
                Style::default().fg(self.palette.success),
            ));
        }

        // Cumulative session usage (wide mode only).
        if self.wide && (d.session_prompt_tokens > 0 || d.session_completion_tokens > 0) {
            right_spans.push(Span::styled(" · ", dim));
            right_spans.push(Span::styled(
                format!("↑{}", format::tokens(d.session_prompt_tokens)),
                Style::default().fg(self.palette.dim),
            ));
            right_spans.push(Span::styled(
                format!(" ↓{}", format::tokens(d.session_completion_tokens)),
                Style::default().fg(self.palette.dim),
            ));
            if d.session_cached_tokens > 0
                && let Some(rate) =
                    format::hit_rate(d.session_cached_tokens, d.session_prompt_tokens)
            {
                // 状态栏要短：整数百分比（摘要有余量，留一位小数）。
                let cache_pct = rate as u32;
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
        // band the right cluster leaves; the first segment that does not fit
        // ends the chain (positions stay stable, nothing jumps around). 左链先
        // 拿到剩余列数：模型段要在自己那一格里先降级（见 `left_chain`）。
        let limit = area.right().saturating_sub(right_width + 1);
        let segments = self.left_chain(limit.saturating_sub(area.x));
        let mut x = area.x;
        for segment in &segments {
            if x + segment.width() > limit {
                break;
            }
            for (span, slot) in &segment.0 {
                let w = span.width() as u16;
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
    /// 理论可命中 token（缓存账本探针）；`0` = 未知（旧网关 / 无参照 / 未计算）。
    pub cache_expected_tokens: i64,
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
            format!("{} in", format::tokens(self.prompt_tokens)),
            dim,
        )]);
        items.push(vec![Span::styled(
            format!("{} out", format::tokens(self.completion_tokens)),
            dim,
        )]);
        if self.prompt_tokens > 0 && (self.cached_tokens > 0 || self.cache_expected_tokens > 0) {
            items.push(vec![Span::styled(self.cache_text(), dim)]);
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

    /// 命中率一格：理论水位已知时成对显示 `67% / 理论 88% cache`，否则**就是今天那
    /// 一个数字**（`66.7% cache`，一位小数）——旧网关上的输出与改动前逐字节一致。
    ///
    /// 状态栏要短，成对形态取整数百分比（摘要有余量，留一位小数）。两个读数都 clamp
    /// 到 `prompt` 以内：超发的手指不该读出 100% 以上。
    fn cache_text(&self) -> String {
        let actual_ratio =
            self.cached_tokens.clamp(0, self.prompt_tokens) as f64 / self.prompt_tokens as f64;
        if self.cache_expected_tokens <= 0 {
            return format!("{:.1}% cache", actual_ratio * 100.0);
        }
        let theory_ratio =
            self.cache_expected_tokens.min(self.prompt_tokens) as f64 / self.prompt_tokens as f64;
        format!(
            "{:.0}% / 理论 {:.0}% cache",
            actual_ratio * 100.0,
            theory_ratio * 100.0
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn test_compact_token_notation() {
        assert_eq!(format::tokens(0), "0");
        assert_eq!(format::tokens(999), "999");
        assert_eq!(format::tokens(1000), "1.0k");
        assert_eq!(format::tokens(12345), "12.3k");
        assert_eq!(format::tokens(1_500_000), "1.5M");
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
            ..TurnUsage::default()
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

    /// 理论水位未知（`cache_expected_tokens == 0`：旧网关 / 无参照）时，命中率那一格
    /// **与改动前逐字节一致**——一位小数、单一数字。这是旧网关上"行为不变"的证据。
    #[test]
    fn test_turn_usage_cache_cell_is_unchanged_when_the_theory_is_unknown() {
        let usage = TurnUsage {
            prompt_tokens: 1200,
            completion_tokens: 340,
            cached_tokens: 800,
            ..TurnUsage::default()
        };
        let items: Vec<String> = usage
            .items(&crate::config::ThemePalette::default())
            .iter()
            .map(|item| item.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(items, vec!["1.2k in", "340 out", "66.7% cache"]);
    }

    /// 理论水位已知 → 实际 / 理论成对（整数百分比，状态栏要短）。
    #[test]
    fn test_turn_usage_pairs_actual_with_the_theory_when_known() {
        let usage = TurnUsage {
            prompt_tokens: 1200,
            completion_tokens: 340,
            cached_tokens: 800,
            cache_expected_tokens: 1056,
            ..TurnUsage::default()
        };
        let items: Vec<String> = usage
            .items(&crate::config::ThemePalette::default())
            .iter()
            .map(|item| item.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        // 800/1200 = 66.67% → 67%；1056/1200 = 88% （与契约样列的 88.1% 同量级）。
        assert_eq!(items, vec!["1.2k in", "340 out", "67% / 理论 88% cache"]);
    }

    /// 理论值超过输入（provider/探针口径差异）不许读出 100% 以上的命中率。
    #[test]
    fn test_turn_usage_theory_is_capped_by_the_prompt() {
        let usage = TurnUsage {
            prompt_tokens: 1000,
            completion_tokens: 10,
            cached_tokens: 1000,
            cache_expected_tokens: 5000,
            ..TurnUsage::default()
        };
        assert_eq!(usage.cache_text(), "100% / 理论 100% cache");
    }

    /// 实际为零但理论已知：**必须**显示（`0% / 理论 99%`）——前缀碎裂、整段重算
    /// 正是这个读数最该在场的一刻；旧行为（`cached == 0` 就不出数）在这里是缺席。
    #[test]
    fn test_turn_usage_shows_a_zero_actual_when_the_theory_is_known() {
        let usage = TurnUsage {
            prompt_tokens: 14_200,
            completion_tokens: 300,
            cached_tokens: 0,
            cache_expected_tokens: 14_000,
            ..TurnUsage::default()
        };
        let items: Vec<String> = usage
            .items(&crate::config::ThemePalette::default())
            .iter()
            .map(|item| item.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(items, vec!["14.2k in", "300 out", "0% / 理论 99% cache"]);
    }

    /// 没有理论水位时 `cached == 0` 仍然不出那一格（与改动前完全一致）。
    #[test]
    fn test_turn_usage_keeps_the_old_gate_without_a_theory() {
        let usage = TurnUsage {
            prompt_tokens: 14_200,
            completion_tokens: 300,
            cached_tokens: 0,
            ..TurnUsage::default()
        };
        let items: Vec<String> = usage
            .items(&crate::config::ThemePalette::default())
            .iter()
            .map(|item| item.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(items, vec!["14.2k in", "300 out"]);
    }

    #[test]
    fn test_status_data_default() {
        let data = StatusData::default();
        assert_eq!(data.model, "unknown");
        assert_eq!(data.session_prompt_tokens, 0);
        assert_eq!(data.session_completion_tokens, 0);
    }

    /// 会话累计（宽模式）：账本基线的三个数渲染为 `↑输入 ↓输出 ◎命中%`。
    #[test]
    fn test_status_bar_shows_cumulative_session_usage_in_wide_mode() {
        let data = StatusData {
            session_prompt_tokens: 120_500,
            session_completion_tokens: 4_200,
            session_cached_tokens: 80_000,
            ..StatusData::default()
        };
        let out = render_text(&data);
        // 80_000 / 120_500 = 66.4% → 状态栏取整为 66%。
        assert!(out.contains("↑120.5k ↓4.2k ◎66%"), "got: {out}");
    }

    /// 窄模式让出这栏；没有输入（prompt 为 0）时不发明命中率。
    #[test]
    fn test_status_bar_usage_needs_width_and_a_prompt_to_hit() {
        let data = StatusData {
            session_prompt_tokens: 1_000,
            session_completion_tokens: 10,
            session_cached_tokens: 500,
            ..StatusData::default()
        };
        assert!(!render_bar(&data, "", 80).0.join("").contains('↑'));

        let no_prompt = StatusData {
            session_prompt_tokens: 0,
            session_cached_tokens: 500,
            ..data
        };
        let out = render_text(&no_prompt);
        assert!(out.contains("↑0"), "got: {out}");
        assert!(!out.contains('◎'), "no prompt → no hit rate: {out}");
    }

    /// The full 24-char session id the fixtures use.
    const SID: &str = "20261005-213012-ab12cd34";

    /// 亮色预设（明暗两套要各断言一次的地方用它）。
    fn light() -> ThemePalette {
        ThemePalette::from_config(&crate::config::ColorsConfig {
            preset: crate::config::ColorPreset::Light,
            ..crate::config::ColorsConfig::default()
        })
    }

    /// 一条"满配"的状态栏数据：24 字符 id + 水位 + 累计用量 + 模式 + provider ——
    /// 右端仪表最肥的一档，优先级与宽度阶梯都在这上面量。
    fn busy() -> StatusData {
        StatusData {
            model: "DeepSeek-Flash".into(),
            provider: Some("dashscope".into()),
            total_tokens: 45_200,
            context_window_tokens: 200_000,
            thinking: true,
            reasoning_effort: Some("medium".into()),
            session_prompt_tokens: 132_400,
            session_completion_tokens: 24_800,
            session_cached_tokens: 98_100,
            ..StatusData::default()
        }
    }

    /// 宽档门槛 = `App::is_wide` 的那条线（终端 ≥ 100 列）。`ui` 不许命名 `app`
    /// 层（`layer_guard`），所以这个数只能在这里重复一份 —— 改门槛时跟着改。
    const WIDE: u16 = 100;

    fn render_buffer_with(
        palette: &ThemePalette,
        data: &StatusData,
        session_id: &str,
        width: u16,
    ) -> (Buffer, StatusBarRegions) {
        let area = Rect::new(0, 0, width, 1);
        let mut buf = Buffer::empty(area);
        let mut regions = StatusBarRegions::default();
        let wide = width >= WIDE;
        StatusBar::new(data, session_id, wide, palette, &mut regions).render(area, &mut buf);
        (buf, regions)
    }

    /// Render the status bar into a buffer and return (one entry per column,
    /// hit regions). Every glyph the bar uses is single-width, so a region's
    /// `x..x+width` slices the very characters it covers.
    fn render_bar(
        data: &StatusData,
        session_id: &str,
        width: u16,
    ) -> (Vec<String>, StatusBarRegions) {
        let (buf, regions) = render_buffer_with(&ThemePalette::default(), data, session_id, width);
        let text = (0..width)
            .map(|x| buf[(x, 0)].symbol().to_string())
            .collect::<Vec<_>>();
        (text, regions)
    }

    /// 一行的格子（符号 / 墨 / 底）：寄存器断言（谁有底、谁是 accent）在文本投影上
    /// 看不出来。
    fn cells_with(
        palette: &ThemePalette,
        data: &StatusData,
        session_id: &str,
        width: u16,
    ) -> Vec<ratatui::buffer::Cell> {
        let (buf, _) = render_buffer_with(palette, data, session_id, width);
        (0..width).map(|x| buf[(x, 0)].clone()).collect()
    }

    /// 命中的列区间（连续的算一段）：`pick` 决定一格算不算。
    fn runs(
        cells: &[ratatui::buffer::Cell],
        pick: impl Fn(&ratatui::buffer::Cell) -> bool,
    ) -> Vec<(usize, usize)> {
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for (x, cell) in cells.iter().enumerate() {
            if !pick(cell) {
                continue;
            }
            match runs.last_mut() {
                Some((_, end)) if *end == x => *end = x + 1,
                _ => runs.push((x, x + 1)),
            }
        }
        runs
    }

    /// 一个列区间的文本。
    fn span_text(cells: &[ratatui::buffer::Cell], (start, end): (usize, usize)) -> String {
        cells[start..end].iter().map(|c| c.symbol()).collect()
    }

    /// 文案断言用：连续空格收成一格。块的内边距是**几何**事实（宽度由下面的
    /// 寄存器测试管），比"某段挨着某段"时不该被它绊住。
    fn single_spaced(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
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
            single_spaced(&out).contains("deepseek-v4-flash-0731 · dashscope-openai"),
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
            !single_spaced(&out).contains("gpt-4 ·"),
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
            single_spaced(&out).contains("DeepSeek-Flash · qoder"),
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
    fn the_model_sits_on_the_rows_only_band() {
        // 模型段用**一块底**说「正在跟谁说话」：底色只出现在这一块，从文首前一格
        // 铺到文末后一格（`Chip::key` 的一格气口），墨是共享件挑的那一档 —— 宽度、
        // 降级、明暗全在 `ui::chip`，这里钉的是"真的用了它"，不是另一份算式。
        // （fixture 是 ASCII：宽字形的尾格在 Buffer 里是 reset 格，CJK 模型名会让
        // `runs` 在每个字形后各切一刀 —— 那是投影的记账方式，不是真机上的缝。）
        for palette in [ThemePalette::default(), light()] {
            let cells = cells_with(&palette, &busy(), SID, 200);
            let bands = runs(&cells, |cell| cell.bg != Color::Reset);
            assert_eq!(
                bands.len(),
                1,
                "{:?}: 整行只有模型那一块有底：{bands:?}",
                palette.appearance
            );
            let expected = Chip::key("DeepSeek-Flash").span(&palette, Pad::One);
            assert_eq!(
                span_text(&cells, bands[0]),
                expected.content.as_ref(),
                "{:?}: 底色铺满内边距",
                palette.appearance
            );
            assert_eq!(cells[bands[0].0].bg, expected.style.bg.expect("底"));
            assert_eq!(cells[bands[0].0].fg, expected.style.fg.expect("墨"));
        }
    }

    #[test]
    fn the_mode_is_the_rows_only_accent() {
        // 模式是整行唯一的强调点：品牌 / 身份 / 模型 / provider / 仪表都不用 accent。
        // 钉住点：pin 上的星标是 `warning`（不与模式抢），没有思考时全行零 accent。
        for palette in [ThemePalette::default(), light()] {
            let cells = cells_with(&palette, &busy(), SID, 200);
            let accents = runs(&cells, |cell| cell.fg == palette.accent);
            assert_eq!(accents.len(), 1, "{:?}：一处 accent", palette.appearance);
            assert_eq!(span_text(&cells, accents[0]), "think:medium");

            let pinned = cells_with(
                &palette,
                &StatusData {
                    pinned: true,
                    ..busy()
                },
                SID,
                200,
            );
            assert_eq!(
                runs(&pinned, |cell| cell.fg == palette.accent).len(),
                1,
                "{:?}: 星标是 warning，不占 accent",
                palette.appearance
            );
            assert_eq!(
                runs(&pinned, |cell| cell.fg == palette.warning).len(),
                1,
                "{:?}: pin 上的星标",
                palette.appearance
            );

            let quiet = cells_with(
                &palette,
                &StatusData {
                    thinking: false,
                    ..busy()
                },
                SID,
                200,
            );
            assert!(
                runs(&quiet, |cell| cell.fg == palette.accent).is_empty(),
                "{:?}: 没有思考就没有强调点",
                palette.appearance
            );
        }
    }

    #[test]
    fn the_model_band_yields_by_the_ladder() {
        // 宽度谱：模型段只有三种结局 —— 有一格气口的块 / 没有气口的块 / 整段不画。
        // 底色宽度永远等于某一档的块（永不半块），模型文字一个不少。标签取得比
        // fixture 长，好让「让出气口」那一档落在谱中间而不是刚好压在宽档门槛上。
        let palette = ThemePalette::default();
        let label = "DeepSeek-Flash-Reasoning";
        let chip = Chip::key(label);
        // 标签的头几个字符（按字符切：换成 CJK 标签时按字节切会 panic）。
        let head: String = label.chars().take(8).collect();
        let mut seen: Vec<usize> = Vec::new();
        for width in 100..=200 {
            let data = StatusData {
                model: label.into(),
                ..busy()
            };
            let cells = cells_with(&palette, &data, SID, width);
            let text: String = cells.iter().map(|cell| cell.symbol()).collect();
            match runs(&cells, |cell| cell.bg != Color::Reset).as_slice() {
                // 一段都放不下：整段不画（不是切成半块，也不是留下半截名字）。
                [] => assert!(
                    !text.contains(&head),
                    "width {width}: 没有底就不该有模型文字：{text:?}"
                ),
                [(start, end)] => {
                    let band = end - start;
                    assert!(
                        band == chip.width(Pad::One) || band == chip.width(Pad::Zero),
                        "width {width}: {band} 列宽的块不是任何一档"
                    );
                    assert_eq!(
                        span_text(&cells, (*start, *end)).trim(),
                        label,
                        "width {width}: 块里的字一个不少"
                    );
                    seen.push(band);
                }
                other => panic!("width {width}: 底色不止一块 {other:?}"),
            }
        }
        assert!(
            seen.contains(&chip.width(Pad::One)),
            "宽档画有一格气口的块：{seen:?}"
        );
        assert!(
            seen.contains(&chip.width(Pad::Zero)),
            "紧档让出气口、字不丢：{seen:?}"
        );
    }

    #[test]
    fn narrow_bars_keep_the_plain_model_span() {
        // 窄档（`wide == false`）不画块：模型是裸字，墨是 `text`、不参与块的宽度数学。
        let palette = ThemePalette::default();
        let cells = cells_with(&palette, &busy(), SID, 80);
        assert!(
            runs(&cells, |cell| cell.bg != Color::Reset).is_empty(),
            "窄终端不画块"
        );
        let text: String = cells.iter().map(|cell| cell.symbol()).collect();
        let at = text.find("DeepSeek-Flash").expect("模型");
        assert_eq!(cells[at].fg, palette.text, "纯文本档的墨");
        assert_eq!(cells[at].bg, Color::Reset);
    }

    #[test]
    fn the_mode_outlives_the_provider_when_the_row_runs_out() {
        // 优先级：模型 → 模式 → provider。120 列 + 满配数据（24 字符 id、累计用量
        // 全满）时 provider 让出、强调点留下；宽到放得下两者时，模式仍然排在
        // provider 之前。
        let tight = render_bar(&busy(), SID, 120).0.join("");
        assert!(tight.contains("think:medium"), "{tight:?}");
        assert!(!tight.contains("dashscope"), "{tight:?}");

        let roomy = render_bar(&busy(), SID, 200).0.join("");
        let mode = roomy.find("think:medium").expect("模式");
        let provider = roomy.find("dashscope").expect("provider");
        assert!(mode < provider, "模式在 provider 之前：{roomy:?}");
    }

    #[test]
    fn a_segment_never_lands_without_its_content() {
        // 截断发生在段边界：分隔符与内容一起进出，所以链条不会以 `·` 收尾，让出的
        // 段也不会留下它的分隔符。
        let forty = render_bar(&busy(), SID, 40).0.join("");
        assert!(
            !forty.contains('·'),
            "身份段整段放不下 → 一个分隔符都不留：{forty:?}"
        );

        let sixty = render_bar(&busy(), SID, 60).0.join("");
        assert!(sixty.contains("…ab12cd34"), "身份段在：{sixty:?}");
        assert_eq!(
            sixty.matches('·').count(),
            1,
            "链条停在星标上，星标之后再没有分隔符：{sixty:?}"
        );
    }

    #[test]
    fn the_copyable_id_outlives_the_pin_star() {
        // ID 与星标各成一段：窄到差一两列时先让星标、ID 还在 —— ID 是「点一下复制」
        // 的唯一可见载体。星标也永远不单独出现：没有 ID 的身份段没有意义。
        let mut held = Vec::new();
        for width in 20..=200 {
            let (text, regions) = render_bar(&busy(), SID, width);
            if regions.star.is_some() {
                assert!(
                    regions.session_id.is_some(),
                    "width {width}: 星标不能单独活着：{text:?}"
                );
            }
            if regions.session_id.is_some() && regions.star.is_none() {
                held.push(width);
            }
        }
        assert!(
            !held.is_empty(),
            "必须存在「ID 在、星标让出」的宽度档（先让星标的优先级）"
        );
    }

    #[test]
    fn the_brand_is_the_rows_only_bold() {
        // 品牌是唯一加粗的一处：其余段（身份 / 模型 / 模式 / provider / 仪表）都靠
        // 底色或色相分层，不靠字重。
        for palette in [ThemePalette::default(), light()] {
            let cells = cells_with(&palette, &busy(), SID, 200);
            let bold = runs(&cells, |cell| {
                cell.modifier.contains(ratatui::style::Modifier::BOLD)
            });
            assert_eq!(bold.len(), 1, "{:?}: 一处加粗", palette.appearance);
            assert_eq!(
                span_text(&cells, bold[0]),
                " Wing",
                "{:?}: 加粗的只有品牌",
                palette.appearance
            );
        }
    }

    #[test]
    fn test_the_thinking_label_carries_the_effort_when_known() {
        // `· think` / `· think:high` —— 中间点与左侧链条同一套分隔符（不然
        // `DeepSeek-Flash MacMachine think` 读起来是三个连写的词）。模式排在
        // provider **之前**：它是全行唯一的强调点，被截断吃掉的强调点不成其为强调点
        // （见 `the_mode_outlives_the_provider_when_the_row_runs_out`）。
        let data = StatusData {
            model: "m".into(),
            provider: Some("p".into()),
            thinking: true,
            ..StatusData::default()
        };
        let (text, _) = render_bar(&data, SID, 120);
        let joined = single_spaced(&text.join(""));
        assert!(joined.contains("m · think · p"), "无档位：{joined:?}");

        let data = StatusData {
            reasoning_effort: Some("high".into()),
            ..data
        };
        let (text, _) = render_bar(&data, SID, 120);
        let joined = single_spaced(&text.join(""));
        assert!(joined.contains("think:high · p"), "带档位：{joined:?}");
    }

    #[test]
    fn test_the_context_meter_reads_zero_on_a_fresh_session() {
        // 空会话的读数必须**看起来是空的**：旧的空轨道用 `░`（25% 阴影块），
        // 暗底上就是一整条灰带 —— `0/500.0k` 因此长得像"用完了"。轨道换成细横线，
        // 分母走容量记法（`500k`，不是 `500.0k`）。
        let data = StatusData {
            model: "m".into(),
            total_tokens: 0,
            context_window_tokens: 500_000,
            ..StatusData::default()
        };
        let (text, _) = render_bar(&data, SID, 120);
        let joined = text.join("");
        assert!(joined.contains("0 / 500k"), "读数：{joined:?}");
        assert!(!joined.contains('░'), "空轨道不该是阴影块：{joined:?}");
        assert!(
            joined.contains(&"─".repeat(10)),
            "10 格空轨道都是细横线：{joined:?}"
        );
    }

    #[test]
    fn test_the_context_meter_fills_with_success_ink() {
        // 半满：5 格实心 + 5 格细线，数字仍是 `100k / 200k`。
        let data = StatusData {
            model: "m".into(),
            total_tokens: 100_000,
            context_window_tokens: 200_000,
            ..StatusData::default()
        };
        let (text, _) = render_bar(&data, SID, 120);
        let joined = text.join("");
        assert!(
            joined.contains(&format!("{}─────", "█".repeat(5))),
            "{joined:?}"
        );
        assert!(joined.contains("100k / 200k"), "读数：{joined:?}");
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
        assert!(
            single_spaced(&text.join("")).contains("Wing · unknown"),
            "{text:?}"
        );
    }

    /// The model identity group (id / call name / provider / label) is applied
    /// as one unit.
    fn model_update(
        id: Option<&str>,
        name: &str,
        provider: Option<&str>,
        label: Option<&str>,
    ) -> ModelUpdate {
        ModelUpdate {
            id: id.map(str::to_string),
            name: name.to_string(),
            provider: provider.map(str::to_string),
            display_name: label.map(str::to_string),
        }
    }

    #[test]
    fn test_apply_session_update_keeps_display_name_with_its_model() {
        let mut data = StatusData::default();
        data.apply_session_update(
            Some(model_update(
                Some("ds-flash"),
                "dfmodel",
                Some("qoder"),
                Some("DeepSeek-Flash"),
            )),
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(data.model, "dfmodel");
        assert_eq!(data.model_id.as_deref(), Some("ds-flash"));
        assert_eq!(data.provider.as_deref(), Some("qoder"));
        assert_eq!(data.model_display_name.as_deref(), Some("DeepSeek-Flash"));
        assert_eq!(data.model_label(), "DeepSeek-Flash");
    }

    #[test]
    fn test_apply_session_update_clears_stale_label_on_model_change() {
        let mut data = StatusData::default();
        data.apply_session_update(
            Some(model_update(
                Some("ds-flash"),
                "dfmodel",
                Some("qoder"),
                Some("DeepSeek-Flash"),
            )),
            None,
            None,
            None,
            None,
            None,
        );
        // Model changes without a declared label: the old label must not
        // survive and describe the new model (nor its provider / id).
        data.apply_session_update(
            Some(model_update(None, "plain", None, None)),
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(data.model, "plain");
        assert_eq!(data.model_id, None);
        assert_eq!(data.provider, None);
        assert_eq!(data.model_display_name, None);
        assert_eq!(data.model_label(), "plain");
    }

    #[test]
    fn test_apply_session_update_without_model_keeps_label() {
        let mut data = StatusData::default();
        data.apply_session_update(
            Some(model_update(
                Some("ds-flash"),
                "dfmodel",
                Some("qoder"),
                Some("DeepSeek-Flash"),
            )),
            None,
            None,
            None,
            None,
            None,
        );
        // thinking-only update: model untouched → identity untouched.
        data.apply_session_update(None, None, None, Some(true), None, None);
        assert_eq!(data.model, "dfmodel");
        assert_eq!(data.model_id.as_deref(), Some("ds-flash"));
        assert_eq!(data.model_display_name.as_deref(), Some("DeepSeek-Flash"));
    }
}
