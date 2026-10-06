//! ThinkingBlock — reasoning content.
//!
//! 两种呈现，由 `rendering.thinking` 给默认、`Ctrl+O` 全局翻转（整条 transcript
//! 一起切，会话内一直有效）；标题行是两者的共同部分：
//!
//! * **标题行**（所有思考块都有）：一行摘要 —— 思考进行时 `⦁ 深度思考中 4s`
//!   持续刷光（光带走灰→白，`sweep_phase` 由帧 tick 推进），结束后定格成
//!   `⦁ 深度思考 12s`；没有计时数据的历史块（重放）只有 `⦁ 深度思考`。
//!   从未按过 `Ctrl+O` 时，**进行中**的行尾带一条静态提示：展开态
//!   `· Ctrl+O 折叠`、折叠态 `· Ctrl+O 展开`（按过一次即退场，见
//!   [`ThinkingBlock::label_line`]）。
//! * **折叠**（`hidden` 的默认）：标题行就是全部可见内容，正文绝不泄露 ——
//!   展开是用户的显式动作。
//! * **展开**（`visible` 的默认，或 `Ctrl+O` 展开后）：标题行作 disclosure
//!   头，markdown 正文整体缩进两列。
//!
//! 展开时的正文渲染：只用正文（text, headings, list markers）继承 thinking
//! 色 —— 换前景、其余修饰符与背景原样保留。代码类与装饰类（行内代码、代码块、
//! 链接、边框、gutter）保留自己的主题色，reasoning 里的围栏代码块与正文里的
//! 同一个块逐 span 相同（syntect 高亮、行号、diff 底色）。

use std::time::Duration;
use std::time::Instant;

use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::config::ThemePalette;
use crate::config::rendering::ThinkingMode;
use crate::render::markdown::ComposedLines;
use crate::render::markdown::ImageOpts;
use crate::render::markdown::Profile;
use crate::render::markdown::RenderOpts;
use crate::render::markdown::compose_lines;
use crate::render::markdown::links::CELL_PREFIX_WIDTH;
use crate::render::markdown::render_markdown_lines_with;
use crate::render::markdown::truncate_to_display_width;
use crate::render::markdown::types::thinking_segment_style;
use crate::ui::shimmer::Rgb;
use crate::ui::shimmer::is_light_theme;
use crate::ui::shimmer::mix;
use crate::ui::shimmer::sweep_intensity;
use crate::ui::shimmer::to_rgb;
use crate::ui::spinner::fmt_elapsed;

/// 标题行进行中的措辞。
const LABEL_ACTIVE: &str = "深度思考中";
/// 标题行结束后的措辞（后接定格时长）。
const LABEL_DONE: &str = "深度思考";
/// 标题行的子弹前缀 —— 与工具调用 / 正文同一套两列词汇。
const LABEL_PREFIX: &str = "⦁ ";
/// 展开态标题行的 Ctrl+O 提示（含与秒数的分隔符）—— 静态、不参与刷光：它是
/// affordance 不是内容；动效只留给「推理进行中」这一个信号。
const LABEL_HINT_COLLAPSE: &str = " · Ctrl+O 折叠";
/// 折叠态标题行的同款提示（指向按下去会发生什么：展开）。
const LABEL_HINT_EXPAND: &str = " · Ctrl+O 展开";

/// 刷光光带半宽（列）。
const SWEEP_RADIUS: f32 = 5.0;
/// 刷光帧间隔（≈25fps）—— 与开屏扫光同一档（系统 tick 的 10fps 会让光带一顿一顿）。
pub const SWEEP_FRAME_STEP: Duration = Duration::from_millis(40);
/// 刷光周期：一趟扫过 + 一段静默（静默由 [`SWEEP_TAIL_GAP`] 决定）。
const SWEEP_PERIOD: Duration = Duration::from_millis(1600);
/// 光带扫出标签后多走的空列 —— 离场后停一拍再回来，持续的刷光才不会显得在抖。
const SWEEP_TAIL_GAP: f32 = 8.0;
/// 高光强度（光带正中，朝高光色混的最大比例）。
const SWEEP_HIGHLIGHT: f32 = 0.75;
/// 暗底主题的高光色（「灰 → 白」的白）。
const DARK_THEME_HIGHLIGHT: Rgb = (255, 255, 255);
/// 亮底主题的"高光"色：亮底上白光看不见，方向翻转成朝深压（同 wordmark）。
const LIGHT_THEME_HIGHLIGHT: Rgb = (10, 14, 24);

/// A block showing agent reasoning/thinking content.
#[derive(Debug, Clone)]
pub struct ThinkingBlock {
    /// Accumulated reasoning text (streamed).
    pub content: String,
    /// 这一块的起止时刻（`None` = 没有计时：重放的历史块）。
    started_at: Option<Instant>,
    ended_at: Option<Instant>,
    /// 帧上显示的已耗时（tick 推进；结束时定格）。
    display_elapsed: Duration,
    /// 刷光相位 0..1（tick 写入；渲染只读 —— 所以渲染是纯函数、可测）。
    sweep_phase: f32,
}

impl ThinkingBlock {
    /// 历史块（重放 / resume）：有正文、没有计时 —— 标题行不带秒数。
    pub fn new() -> Self {
        Self {
            content: String::new(),
            started_at: None,
            ended_at: None,
            display_elapsed: Duration::ZERO,
            sweep_phase: 0.0,
        }
    }

    /// Append reasoning content (streaming).
    pub fn append(&mut self, text: &str) {
        self.content.push_str(text);
    }

    /// 开始计时（live 流的第一条 reasoning 落在这一块上时）。
    pub fn start(&mut self, now: Instant) {
        if self.started_at.is_none() {
            self.started_at = Some(now);
        }
    }

    /// 计时是否在跑（标题行显示进行中措辞 + 刷光）。
    pub fn is_active(&self) -> bool {
        self.started_at.is_some() && self.ended_at.is_none()
    }

    /// 这一段推理是否已经收尾（冻结过）—— 新到的 reasoning 属于下一段。
    pub fn is_finished(&self) -> bool {
        self.ended_at.is_some()
    }

    /// 推到 `now`：更新显示的耗时与刷光相位。帧 tick 每帧调一次。
    pub fn tick(&mut self, now: Instant) {
        let Some(started) = self.started_at else {
            return;
        };
        if self.ended_at.is_some() {
            return;
        }
        self.display_elapsed = now.saturating_duration_since(started);
        let period = SWEEP_PERIOD.as_secs_f32();
        self.sweep_phase = (self.display_elapsed.as_secs_f32() / period) % 1.0;
    }

    /// 定格：模型进入下一阶段（正文 / 工具调用），或回合结束 —— 时长冻结。
    ///
    /// 幂等：晚到的第二个冻结点不会把时长往后拉。
    pub fn finish(&mut self, now: Instant) {
        let Some(started) = self.started_at else {
            return;
        };
        if self.ended_at.is_some() {
            return;
        }
        self.display_elapsed = now.saturating_duration_since(started);
        self.ended_at = Some(now);
    }

    /// 直接摆刷光相位（0..1，自动取模）—— 动画帧由 [`Self::tick`] 推进；
    /// 预览 example / 测试用它把光带定格到某处。
    pub fn set_sweep_phase(&mut self, phase: f32) {
        self.sweep_phase = phase.rem_euclid(1.0);
    }

    /// 下一帧的**绝对**截止时刻（`None` = 不在计时 / 已冻结）。
    ///
    /// 绝对时刻而不是"睡 `step`"：事件循环里任何事件都会重建这个 future，
    /// 相对 sleep 会被流式事件无限推后，光带就卡死在原地（同欢迎屏
    /// `Welcome::next_frame` 的契约）。网格锚在块的起点上，逐帧严格递增。
    pub fn next_frame(&self, now: Instant, step: Duration) -> Option<Instant> {
        let started = self.started_at?;
        if self.ended_at.is_some() {
            return None;
        }
        let step_ms = step.as_millis().max(1);
        let elapsed = now.saturating_duration_since(started).as_millis();
        let due_ms = (elapsed / step_ms + 1).saturating_mul(step_ms);
        let due_ms = u64::try_from(due_ms).unwrap_or(u64::MAX);
        Some(started + Duration::from_millis(due_ms))
    }

    /// 标题行的显示文本：措辞（进行中 / 已结束）+ 时长后缀。
    ///
    /// 不满一秒不带秒数（`0s` 没意义）—— 进行中与定格同口径；没有计时数据
    /// 的历史块（重放）只有措辞。
    fn label_texts(&self) -> (String, String) {
        let seconds = self.display_elapsed.as_secs();
        let active = self.is_active();
        let head = if active {
            format!("{LABEL_PREFIX}{LABEL_ACTIVE}")
        } else {
            format!("{LABEL_PREFIX}{LABEL_DONE}")
        };
        let tail = if self.started_at.is_none() || seconds == 0 {
            String::new()
        } else {
            format!(" {}", fmt_elapsed(seconds))
        };
        (head, tail)
    }

    /// 标题行（折叠时的全部可见内容，展开时的 disclosure 头）。`width` 是
    /// 内容宽度：标题在任何宽度下都必须 ≤ width（展开时它走 blit 路径，
    /// 超宽会被裁而不是换行）。
    ///
    /// 布局：`⦁ 深度思考中`（刷光）+ 静态秒数 +（条件性）静态 `Ctrl+O` 提示。
    /// 让位序：**提示最先让位**（整条装不下就不显示）→ 秒数 → 措辞 ——
    /// 极端窄屏下宁可只剩一个词，也不让标题行溢出。
    ///
    /// 提示的显示条件：本块**进行中**、且这次会话**还没按过** `Ctrl+O`
    /// （`explicit.is_none()`）—— 按过一次（模式已全局翻转、切换效果自证）
    /// 或块已冻结（历史行保持安静）就不再出现。文案指向**按下去会发生什么**
    /// （由 `mode` + `explicit` 解析出的当前展开态）：展开中 `· Ctrl+O 折叠`、
    /// 折叠中 `· Ctrl+O 展开`。
    pub fn label_line(
        &self,
        palette: &ThemePalette,
        width: u16,
        mode: ThinkingMode,
        explicit: Option<bool>,
    ) -> Line<'static> {
        let dim = Style::default().fg(palette.dim);
        let (mut head, mut tail) = self.label_texts();
        let budget = width as usize;
        // 提示：静态、不进刷光跨度（刷光只扫措辞，见 `shine_spans`）。
        let mut hint =
            (self.is_active() && explicit.is_none()).then_some(if mode.expanded(explicit) {
                LABEL_HINT_COLLAPSE
            } else {
                LABEL_HINT_EXPAND
            });
        if let Some(candidate) = hint
            && UnicodeWidthStr::width(head.as_str())
                + UnicodeWidthStr::width(tail.as_str())
                + UnicodeWidthStr::width(candidate)
                > budget
        {
            hint = None;
        }
        head = truncate_to_display_width(
            &head,
            budget.saturating_sub(UnicodeWidthStr::width(tail.as_str())),
        );
        if UnicodeWidthStr::width(head.as_str()) + UnicodeWidthStr::width(tail.as_str()) > budget {
            tail.clear();
            head = truncate_to_display_width(&head, budget);
        }
        let mut spans = self.shine_spans(&head, palette, dim);
        // 静止部分跟措辞同一色系：进行中措辞是逐列 Rgb 混色的（见
        // `shine_spans`），秒数与提示就用 dim 的 Rgb 解析值 —— 命名色与
        // Rgb 混用会在命名色主题 + 非 truecolor 终端上各走各的色（评审 N）。
        // 定格后整行回到命名 dim，与旧行为一致。
        let static_style = if self.is_active() {
            let base = to_rgb(palette.dim);
            Style::default().fg(Color::Rgb(base.0, base.1, base.2))
        } else {
            dim
        };
        if !tail.is_empty() {
            spans.push(Span::styled(tail, static_style));
        }
        if let Some(hint) = hint {
            spans.push(Span::styled(hint.to_string(), static_style));
        }
        Line::from(spans)
    }

    /// 措辞的逐列刷光：进行中按 `sweep_phase` 混高光色；否则整段静态。
    fn shine_spans(&self, text: &str, palette: &ThemePalette, dim: Style) -> Vec<Span<'static>> {
        if !self.is_active() {
            return vec![Span::styled(text.to_string(), dim)];
        }
        let base = to_rgb(palette.dim);
        let peak = if is_light_theme(to_rgb(palette.text)) {
            LIGHT_THEME_HIGHLIGHT
        } else {
            DARK_THEME_HIGHLIGHT
        };
        // 光带的跨度 = 措辞宽度 + 尾部空列：扫出去后停一拍再回来。
        let span = UnicodeWidthStr::width(text) as f32 + SWEEP_TAIL_GAP;
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(text.chars().count());
        let mut column = 0.0f32;
        for ch in text.chars() {
            let width = UnicodeWidthChar::width(ch).unwrap_or(0) as f32;
            let intensity =
                sweep_intensity(column + width / 2.0, self.sweep_phase, span, SWEEP_RADIUS);
            let rgb = mix(base, peak, SWEEP_HIGHLIGHT * intensity);
            let color = Color::Rgb(rgb.0, rgb.1, rgb.2);
            // 同色的字符合并成一个 span（相邻列强度不同 -> 颜色不同 -> 不会合并）。
            match spans.last_mut() {
                Some(last) if last.style.fg == Some(color) => last.content.to_mut().push(ch),
                _ => spans.push(Span::styled(ch.to_string(), Style::default().fg(color))),
            }
            column += width;
        }
        spans
    }

    /// Render to lines based on thinking mode.
    ///
    /// `mode` is the configured default and `explicit` the Ctrl+O override
    /// (`None` = follow the default) — they resolve through
    /// [`ThinkingMode::expanded`]. `width` is the full content width; 2
    /// columns are reserved for the line prefix so tables balance to fit.
    /// `images` carries the frame's image options — reasoning gets the same
    /// anchors as assistant content when the markdown layer sees them (see
    /// `render/markdown/images.rs`).
    pub fn to_lines(
        &self,
        palette: &ThemePalette,
        mode: ThinkingMode,
        explicit: Option<bool>,
        width: u16,
        images: &ImageOpts,
    ) -> Vec<Line<'static>> {
        self.render_lines(palette, mode, explicit, width, images)
            .into_lines()
    }

    /// [`to_lines`](Self::to_lines) with the markdown link spans of every
    /// rendered line (展开时才可能有链接；折叠的标题行没有）。
    pub fn render_lines(
        &self,
        palette: &ThemePalette,
        mode: ThinkingMode,
        explicit: Option<bool>,
        width: u16,
        images: &ImageOpts,
    ) -> ComposedLines {
        let header = self.label_line(palette, width, mode, explicit);
        if !mode.expanded(explicit) {
            let mut composed = ComposedLines::plain(vec![header]);
            composed.push_blank();
            return composed;
        }
        // 展开：标题行作 disclosure 头，正文整体用两列续行缩进。
        let mut composed = self.content_lines(palette, width, images);
        composed = with_header(composed, header);
        composed.push_blank();
        composed
    }

    /// 展开的正文（markdown，thinking profile）—— 每行都拿两列续行缩进
    /// （子弹 `⦁ ` 归标题行，由 [`with_header`] 装上）。
    fn content_lines(
        &self,
        palette: &ThemePalette,
        width: u16,
        images: &ImageOpts,
    ) -> ComposedLines {
        let thinking_style = Style::default().fg(palette.thinking);
        let md_width = Some(width.saturating_sub(2));
        // Code blocks render exactly like assistant content (syntect +
        // gutter); only the prose is recolored to the thinking color — see
        // `Profile` for what reasoning changes, and what it does not.
        let md_lines = render_markdown_lines_with(
            &self.content,
            md_width,
            palette,
            RenderOpts::new(Profile::Thinking, true)
                .with_math(palette.math_mode)
                .with_images(images),
        );
        compose_lines(
            &md_lines,
            CELL_PREFIX_WIDTH,
            |_i| Span::styled("  ", thinking_style),
            |kind, style| thinking_segment_style(kind, style, thinking_style),
        )
    }
}

/// 在正文前插入标题行（链接 / 图片侧通道保持逐行对齐）。
fn with_header(composed: ComposedLines, header: Line<'static>) -> ComposedLines {
    let (mut lines, mut links, mut images) = composed.into_parts();
    // 标题占第 0 行：既有锚点的**绝对**行号（行号 == 索引）跟着下移一行，
    // 否则绘制侧会按错行找 caption、拒绘 —— 图片静默消失。
    crate::render::markdown::images::shift_anchors_after_insert(&mut images, 0);
    lines.insert(0, header);
    links.insert(0, Vec::new());
    images.insert(0, Vec::new());
    ComposedLines::with_images(lines, links, images)
}

impl Default for ThinkingBlock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::rendering::ThinkingMode;
    use crate::render::markdown::SegmentKind;
    use ratatui::style::Modifier;

    fn p() -> ThemePalette {
        ThemePalette::default()
    }

    fn t0() -> Instant {
        Instant::now()
    }

    /// Collect (text, style) pairs across all rendered spans.
    fn span_pairs(lines: &[Line<'static>]) -> Vec<(String, Style)> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| (s.content.to_string(), s.style))
            .collect()
    }

    fn find_span<'a>(pairs: &'a [(String, Style)], needle: &str) -> &'a (String, Style) {
        pairs
            .iter()
            .find(|(text, _)| text.contains(needle))
            .unwrap_or_else(|| panic!("span containing {needle:?} not found: {pairs:?}"))
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn text_of(lines: &[Line<'static>]) -> String {
        lines.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    // ── 标题行 ───────────────────────────────────────────────

    #[test]
    fn hidden_label_shows_live_seconds() {
        let mut block = ThinkingBlock::new();
        block.append("secret reasoning");
        let start = t0();
        block.start(start);
        block.tick(start + Duration::from_millis(300));
        let lines = block.to_lines(&p(), ThinkingMode::Hidden, None, 80, ImageOpts::off());
        let text = text_of(&lines);
        // 第一秒内不显示 0s：只有措辞。
        assert!(text.contains("深度思考中"), "{text}");
        assert!(!text.contains("0s"), "不满一秒不显示秒数：{text}");
        assert!(!text.contains("secret"), "折叠不泄露正文：{text}");

        block.tick(start + Duration::from_secs(4));
        let text = text_of(&block.to_lines(&p(), ThinkingMode::Hidden, None, 80, ImageOpts::off()));
        assert!(text.contains("深度思考中 4s"), "{text}");
    }

    #[test]
    fn hidden_label_hints_the_toggle_until_first_use() {
        let mut block = ThinkingBlock::new();
        let start = t0();
        block.start(start);
        block.tick(start + Duration::from_secs(4));
        // 进行中 + 从未按过 Ctrl+O：静态提示在场。
        let text = text_of(&block.to_lines(&p(), ThinkingMode::Hidden, None, 80, ImageOpts::off()));
        assert!(text.contains("深度思考中 4s · Ctrl+O 展开"), "{text}");

        // 按过一次（模式已有显式值）就退场 —— 展开出的标题行与收起态都不带。
        for explicit in [Some(true), Some(false)] {
            let text = text_of(&block.to_lines(
                &p(),
                ThinkingMode::Hidden,
                explicit,
                80,
                ImageOpts::off(),
            ));
            assert!(!text.contains("Ctrl+O"), "{explicit:?}: {text}");
        }

        // 冻结的历史行保持安静。
        block.finish(start + Duration::from_secs(9));
        let text = text_of(&block.to_lines(&p(), ThinkingMode::Hidden, None, 80, ImageOpts::off()));
        assert!(text.contains("深度思考 9s"), "{text}");
        assert!(!text.contains("Ctrl+O"), "{text}");

        // 重放的历史块（没有计时、不 active）也不带提示。
        let replayed = ThinkingBlock::new();
        let text =
            text_of(&replayed.to_lines(&p(), ThinkingMode::Hidden, None, 80, ImageOpts::off()));
        assert!(!text.contains("Ctrl+O"), "{text}");
    }

    #[test]
    fn hidden_label_hint_is_static_and_gives_way_first() {
        let mut block = ThinkingBlock::new();
        let start = t0();
        block.start(start);
        block.tick(start + Duration::from_secs(4));

        // 提示是静态 dim：不参与刷光（单色 span，不是逐列混光的那些）。
        let pairs =
            span_pairs(&block.to_lines(&p(), ThinkingMode::Hidden, None, 80, ImageOpts::off()));
        let (_, hint_style) = find_span(&pairs, "Ctrl+O");
        assert_eq!(
            to_rgb(hint_style.fg.expect("提示有颜色")),
            to_rgb(p().dim),
            "提示是静止的 affordance，不该跟着刷光：{hint_style:?}"
        );

        // 让位序：提示最先让位 —— 正好装下时在，少一列就整个消失，
        // 措辞与秒数照旧（宁可少提示，不让内容先让位）。
        let full_lines = block.to_lines(&p(), ThinkingMode::Hidden, None, 80, ImageOpts::off());
        let tight_width = UnicodeWidthStr::width(line_text(&full_lines[0]).as_str()) as u16;
        let tight = text_of(&block.to_lines(
            &p(),
            ThinkingMode::Hidden,
            None,
            tight_width,
            ImageOpts::off(),
        ));
        assert!(tight.contains("Ctrl+O 展开"), "正好装下时应保留：{tight}");
        let narrow = text_of(&block.to_lines(
            &p(),
            ThinkingMode::Hidden,
            None,
            tight_width - 1,
            ImageOpts::off(),
        ));
        assert!(
            !narrow.contains("Ctrl+O"),
            "容不下时提示第一个让位：{narrow}"
        );
        assert!(
            narrow.contains("深度思考中 4s"),
            "措辞与秒数不该为提示让位：{narrow}"
        );
    }

    #[test]
    fn hidden_label_freezes_into_a_duration() {
        let mut block = ThinkingBlock::new();
        block.append("reasoning");
        let start = t0();
        block.start(start);
        block.tick(start + Duration::from_secs(3));
        block.finish(start + Duration::from_secs(12));

        let text = text_of(&block.to_lines(&p(), ThinkingMode::Hidden, None, 80, ImageOpts::off()));
        assert!(text.contains("深度思考 12s"), "{text}");
        assert!(!text.contains("深度思考中"), "结束后不再说进行中：{text}");
        assert!(!block.is_active());

        // 幂等 + 定格：再 tick / finish 都不动。
        block.tick(start + Duration::from_secs(30));
        block.finish(start + Duration::from_secs(30));
        let text = text_of(&block.to_lines(&p(), ThinkingMode::Hidden, None, 80, ImageOpts::off()));
        assert!(
            text.contains("深度思考 12s"),
            "时长被后续事件拉长了：{text}"
        );
    }

    #[test]
    fn hidden_label_omits_a_zero_second_duration() {
        // 不满一秒就定格：与进行中同口径，不显示 `0s`（评审 N）。
        let mut block = ThinkingBlock::new();
        let start = t0();
        block.start(start);
        block.finish(start + Duration::from_millis(400));
        let text = text_of(&block.to_lines(&p(), ThinkingMode::Hidden, None, 80, ImageOpts::off()));
        assert!(text.contains("深度思考"), "{text}");
        assert!(!text.contains("深度思考中"), "已定格：{text}");
        assert!(!text.contains('0'), "0s 不该出现：{text}");
    }

    #[test]
    fn hidden_label_without_timing_has_no_seconds() {
        let mut block = ThinkingBlock::new();
        block.append("replayed reasoning");
        let text = text_of(&block.to_lines(&p(), ThinkingMode::Hidden, None, 80, ImageOpts::off()));
        assert!(text.contains("深度思考"), "{text}");
        assert!(!text.contains("深度思考中"), "{text}");
        assert!(!text.contains('s'), "没有计时数据就不该出现秒数：{text}");
    }

    #[test]
    fn hidden_label_shines_towards_white_while_active() {
        let mut block = ThinkingBlock::new();
        block.append("x");
        let start = t0();
        block.start(start);
        // 4s = 2.5 个周期 → 相位 0.5（光带在措辞中部），秒数也已显示。
        block.tick(start + Duration::from_secs(4));

        let base = to_rgb(p().dim);
        let sum = |rgb: Rgb| rgb.0 as u32 + rgb.1 as u32 + rgb.2 as u32;
        let pairs =
            span_pairs(&block.to_lines(&p(), ThinkingMode::Hidden, None, 80, ImageOpts::off()));
        let colored = |pairs: &[(String, Style)]| -> Vec<(String, Rgb)> {
            pairs
                .iter()
                .filter_map(|(text, style)| style.fg.map(|c| (text.clone(), to_rgb(c))))
                .collect()
        };
        let colors = colored(&pairs);
        let brightest = colors
            .iter()
            .map(|(_, rgb)| *rgb)
            .max_by_key(|rgb| sum(*rgb))
            .expect("有颜色");
        assert!(
            sum(brightest) > sum(base),
            "进行中应有一段比底色亮（灰→白）：{brightest:?} vs {base:?}"
        );
        // 秒数保持静态 dim：它不在刷光带里。
        let (_, secs_color) = colors
            .iter()
            .find(|(text, _)| text.contains('s'))
            .expect("秒数应已显示");
        assert_eq!(*secs_color, base, "秒数不该跟着刷光");
    }

    #[test]
    fn hidden_label_static_when_done() {
        let mut block = ThinkingBlock::new();
        block.append("x");
        let start = t0();
        block.start(start);
        block.finish(start + Duration::from_secs(5));
        let pairs =
            span_pairs(&block.to_lines(&p(), ThinkingMode::Hidden, None, 80, ImageOpts::off()));
        let base = to_rgb(p().dim);
        for (text, style) in &pairs {
            if let Some(color) = style.fg {
                assert_eq!(to_rgb(color), base, "定格后不该有高光（{text}）");
            }
        }
    }

    #[test]
    fn hidden_label_fits_any_width() {
        let mut block = ThinkingBlock::new();
        block.append("x");
        let start = t0();
        block.start(start);
        block.tick(start + Duration::from_secs(65));
        for width in [40u16, 20, 12, 8, 4] {
            let lines = block.to_lines(&p(), ThinkingMode::Hidden, None, width, ImageOpts::off());
            for line in &lines {
                assert!(
                    line.width() <= width as usize,
                    "宽度 {width}：行超宽 {:?}",
                    line_text(line)
                );
            }
        }
    }

    #[test]
    fn hidden_label_pulses_on_a_loop() {
        // 刷光相位循环：一个周期后回到同一相位（持续刷光而不是扫一次就停）。
        let mut block = ThinkingBlock::new();
        let start = t0();
        block.start(start);
        block.tick(start + Duration::from_millis(100));
        let first = block.sweep_phase;
        block.tick(start + Duration::from_millis(100) + SWEEP_PERIOD);
        assert!((block.sweep_phase - first).abs() < 1e-6);
    }

    #[test]
    fn next_frame_grid_is_absolute_and_strictly_future() {
        // 「绝对帧网格」契约：事件循环里任何事件都会重建定时 future，相对 sleep
        // 会被无限推后 —— 所以下一帧必须锚在块的起点上、且严格在未来。
        let start = t0();
        let mut block = ThinkingBlock::new();
        block.start(start);
        let step = Duration::from_millis(40);
        for offset in [0u64, 1, 39, 40, 41, 199] {
            let now = start + Duration::from_millis(offset);
            let due = block.next_frame(now, step).expect("活跃块有下一帧");
            assert!(due > now, "截止时刻必须严格在未来：{due:?} vs {now:?}");
            let ms = due.saturating_duration_since(start).as_millis();
            assert_eq!(
                ms % 40,
                0,
                "帧网格必须锚在块的起点上（{offset}ms -> {ms}ms）"
            );
            assert!(
                ms <= u128::from(offset + 40),
                "不该跳过一个整帧（{offset}ms -> {ms}ms）"
            );
        }
        // 冻结后不再排帧；没有计时数据的历史块（重放）也永远不排。
        block.finish(start + Duration::from_secs(1));
        assert_eq!(block.next_frame(start + Duration::from_secs(1), step), None);
        assert_eq!(ThinkingBlock::new().next_frame(start, step), None);
    }

    // ── 展开 / 标题 ──────────────────────────────────────────

    #[test]
    fn expanded_keeps_the_label_as_a_header() {
        let mut block = ThinkingBlock::new();
        block.append("the reasoning body");
        let lines = block.to_lines(&p(), ThinkingMode::Hidden, Some(true), 80, ImageOpts::off());
        assert_eq!(line_text(&lines[0]), "⦁ 深度思考", "标题行丢失");
        assert!(
            line_text(&lines[1]).starts_with("  "),
            "展开正文应缩进两列：{:?}",
            line_text(&lines[1])
        );
        assert!(
            lines
                .iter()
                .any(|l| line_text(l).contains("reasoning body"))
        );
    }

    /// 非流式路径（resume 重放 / 收起再展开）：标题占第 0 行、正文整体下移
    /// 一行 —— 图片锚点的**绝对**行号必须跟着下移。不平移的话绘制侧
    /// `caption_at` 会按错行找 caption、拒绘，图片静默消失（评审 B）。
    #[test]
    fn expanded_header_keeps_the_image_anchor_on_its_caption_row() {
        use crate::render::markdown::CellPixels;
        use crate::render::markdown::images::ImageEntry;
        use crate::render::markdown::images::ImageShape;

        let root = std::path::PathBuf::from("/ws");
        let images = ImageOpts::anchor(
            Some(root.clone()),
            vec![ImageEntry::new(
                root.join("plot.png"),
                ImageShape::new(800, 600),
            )],
            CellPixels::new(10, 20),
        );
        let mut block = ThinkingBlock::new();
        block.append("intro\n\n![plot](./plot.png)\n\ntail");
        let composed = block.render_lines(&p(), ThinkingMode::Hidden, Some(true), 80, &images);
        let (lines, _links, side) = composed.into_parts();
        assert!(
            line_text(&lines[0]).contains("深度思考"),
            "标题应在第 0 行：{:?}",
            line_text(&lines[0])
        );
        let anchors: Vec<(usize, &crate::render::markdown::images::ImageSpan)> = side
            .iter()
            .enumerate()
            .flat_map(|(index, spans)| spans.iter().map(move |span| (index, span)))
            .collect();
        assert_eq!(anchors.len(), 1, "夹具里应有 1 个锚点：{anchors:?}");
        let (index, span) = anchors[0];
        assert_eq!(span.line, index, "锚点行号必须等于条目下标");
        assert!(
            line_text(&lines[span.line]).contains('▢'),
            "锚点应落在 caption 行：{:?}",
            line_text(&lines[span.line])
        );
    }

    #[test]
    fn visible_default_keeps_the_label_as_a_header() {
        // `rendering.thinking = visible`（没按过 Ctrl+O）：标题行在上、正文
        // 展开在下 —— 标题拿 `⦁ `，正文两列续行缩进。
        let mut block = ThinkingBlock::new();
        block.append("plain reasoning");
        let lines = block.to_lines(&p(), ThinkingMode::Visible, None, 80, ImageOpts::off());
        let header = line_text(&lines[0]);
        assert!(header.starts_with("⦁ 深度思考"), "{header:?}");
        let body = line_text(&lines[1]);
        assert!(body.starts_with("  plain reasoning"), "{body:?}");
        assert!(
            !header.contains("plain reasoning"),
            "正文不该挤进标题行：{header:?}"
        );
    }

    #[test]
    fn visible_default_hints_collapse_while_active() {
        // 默认展开 + 进行中 + 从未按过 Ctrl+O：标题行尾提示指向「按下去会折叠」。
        let mut block = ThinkingBlock::new();
        block.append("body");
        let start = t0();
        block.start(start);
        block.tick(start + Duration::from_secs(4));
        let text =
            text_of(&block.to_lines(&p(), ThinkingMode::Visible, None, 80, ImageOpts::off()));
        assert!(text.contains("深度思考中 4s · Ctrl+O 折叠"), "{text}");

        // 按过一次（模式已有显式值）就退场 —— 两个方向都不带。
        for explicit in [Some(true), Some(false)] {
            let text = text_of(&block.to_lines(
                &p(),
                ThinkingMode::Visible,
                explicit,
                80,
                ImageOpts::off(),
            ));
            assert!(!text.contains("Ctrl+O"), "{explicit:?}: {text}");
        }

        // 冻结的历史行保持安静。
        block.finish(start + Duration::from_secs(9));
        let text =
            text_of(&block.to_lines(&p(), ThinkingMode::Visible, None, 80, ImageOpts::off()));
        assert!(text.contains("深度思考 9s"), "{text}");
        assert!(!text.contains("Ctrl+O"), "{text}");
    }

    #[test]
    fn toggle_collapses_a_visible_default_block() {
        let mut block = ThinkingBlock::new();
        block.append("SECRET-REASONING");
        let text = text_of(&block.to_lines(
            &p(),
            ThinkingMode::Visible,
            Some(false),
            80,
            ImageOpts::off(),
        ));
        assert!(text.contains("深度思考"), "{text}");
        assert!(!text.contains("SECRET"), "收起后正文不该泄露：{text}");
    }

    #[test]
    fn hidden_expanded_matches_the_visible_default_span_for_span() {
        // 展开的两条路径 —— `hidden` + Ctrl+O 展开、`visible` 默认 —— 渲染
        // 同一形态（标题 + 两列缩进正文）；唯一的差别是标题行尾的 Ctrl+O
        // 提示（hidden 一侧已按过键、提示退场；visible 一侧指向「折叠」）。
        let start = t0();
        let body = "here is `code` and text\n\n- item";
        let mut a = ThinkingBlock::new();
        a.append(body);
        a.start(start);
        a.tick(start + Duration::from_secs(4));
        let mut b = ThinkingBlock::new();
        b.append(body);
        b.start(start);
        b.tick(start + Duration::from_secs(4));
        let expanded = a.to_lines(&p(), ThinkingMode::Hidden, Some(true), 80, ImageOpts::off());
        let visible = b.to_lines(&p(), ThinkingMode::Visible, None, 80, ImageOpts::off());
        assert_eq!(
            expanded.len(),
            visible.len(),
            "两条路径同形态（标题 + 正文）"
        );
        assert_eq!(line_text(&expanded[0]), "⦁ 深度思考中 4s");
        assert_eq!(line_text(&visible[0]), "⦁ 深度思考中 4s · Ctrl+O 折叠");
        // 标题行：visible 侧只多一个提示 span（含逐列混色的措辞在内，其余
        // 逐 span 相同 —— 两边相位与颜色换算完全一致）。
        let e_header: Vec<_> = expanded[0]
            .spans
            .iter()
            .map(|s| (s.content.to_string(), s.style))
            .collect();
        let v_header: Vec<_> = visible[0]
            .spans
            .iter()
            .map(|s| (s.content.to_string(), s.style))
            .collect();
        assert_eq!(v_header.len(), e_header.len() + 1, "只多提示一个 span");
        assert_eq!(
            v_header[..e_header.len()],
            e_header[..],
            "提示之前的标题 span 相同"
        );
        assert_eq!(v_header.last().unwrap().0, " · Ctrl+O 折叠");
        // 正文（第 1 行起）逐 span 相同。
        for (e, v) in expanded.iter().skip(1).zip(visible.iter().skip(1)) {
            let e_pairs: Vec<_> = e
                .spans
                .iter()
                .map(|s| (s.content.to_string(), s.style))
                .collect();
            let v_pairs: Vec<_> = v
                .spans
                .iter()
                .map(|s| (s.content.to_string(), s.style))
                .collect();
            assert_eq!(e_pairs, v_pairs, "正文行必须逐 span 相同");
        }
    }

    // ── 正文渲染（与旧行为一致的部分）────────────────────────

    #[test]
    fn test_thinking_renders_content() {
        let mut block = ThinkingBlock::new();
        block.append("Let me think about this...");
        let lines = block.to_lines(&p(), ThinkingMode::Visible, None, 80, ImageOpts::off());
        let text = text_of(&lines);
        assert!(text.contains("⦁ 深度思考"), "missing header: {text}");
        assert!(text.contains("Let me think"), "missing content: {text}");
        // Header on top, body below (two-column continuation indent).
        assert!(line_text(&lines[0]).starts_with("⦁ 深度思考"), "{text}");
        assert!(line_text(&lines[1]).starts_with("  Let me think"), "{text}");
        assert!(!text.contains("│"), "should not have border: {text}");
    }

    #[test]
    fn test_thinking_prose_uses_thinking_color() {
        let mut block = ThinkingBlock::new();
        block.append("plain reasoning text");
        let lines = block.to_lines(&p(), ThinkingMode::Visible, None, 80, ImageOpts::off());
        let pairs = span_pairs(&lines);
        let (_, style) = find_span(&pairs, "plain reasoning");
        assert_eq!(style.fg, Some(p().thinking), "prose fg: {style:?}");
    }

    #[test]
    fn test_thinking_keeps_inline_code_color() {
        let mut block = ThinkingBlock::new();
        block.append("run `cargo build` now");
        let lines = block.to_lines(&p(), ThinkingMode::Visible, None, 80, ImageOpts::off());
        let pairs = span_pairs(&lines);
        // Inline code keeps the accent color.
        let (_, code_style) = find_span(&pairs, "cargo build");
        assert_eq!(code_style.fg, Some(p().accent), "code fg: {code_style:?}");
        // Surrounding prose is recolored to the thinking color.
        let (_, prose_style) = find_span(&pairs, "run ");
        assert_eq!(
            prose_style.fg,
            Some(p().thinking),
            "prose fg: {prose_style:?}"
        );
    }

    #[test]
    fn test_thinking_bold_keeps_modifier_with_gray_fg() {
        let mut block = ThinkingBlock::new();
        block.append("this is **important** indeed");
        let lines = block.to_lines(&p(), ThinkingMode::Visible, None, 80, ImageOpts::off());
        let pairs = span_pairs(&lines);
        let (_, style) = find_span(&pairs, "important");
        assert_eq!(style.fg, Some(p().thinking), "bold fg: {style:?}");
        assert!(
            style.add_modifier.contains(Modifier::BOLD),
            "bold modifier lost: {style:?}"
        );
    }

    #[test]
    fn test_thinking_prose_recolor_preserves_bg_and_sub_modifier() {
        // Only the foreground is swapped; bg and sub-modifiers (set via
        // remove_modifier) must survive untouched.
        let original = Style::default()
            .bg(Color::Red)
            .remove_modifier(Modifier::ITALIC);
        let out = thinking_segment_style(
            SegmentKind::Text,
            original,
            Style::default().fg(Color::Gray),
        );
        assert_eq!(out.fg, Some(Color::Gray), "fg not swapped: {out:?}");
        assert_eq!(out.bg, Some(Color::Red), "bg lost: {out:?}");
        assert!(
            out.sub_modifier.contains(Modifier::ITALIC),
            "sub_modifier lost: {out:?}"
        );
    }

    #[test]
    fn test_thinking_code_kind_keeps_style_verbatim() {
        let original = Style::default().fg(Color::Cyan).bold();
        for kind in [
            SegmentKind::InlineCode,
            SegmentKind::CodeBlock,
            SegmentKind::Link,
            SegmentKind::Border,
            SegmentKind::Gutter,
            // Math and image anchors are structural like code: a formula or a
            // caption keeps its own color inside reasoning.
            SegmentKind::Math,
            SegmentKind::Image,
        ] {
            let out = thinking_segment_style(kind, original, Style::default().fg(Color::Gray));
            assert_eq!(out, original, "kind {kind:?} should keep its style");
        }
    }

    /// The thinking cell reads the math mode off the palette (like the
    /// assistant-message cell and the streaming engine): `off` leaves the
    /// LaTeX source verbatim inside reasoning too.
    #[test]
    fn test_thinking_follows_the_palette_math_mode() {
        use crate::config::rendering::MathMode;

        let mut block = ThinkingBlock::new();
        block.append("the energy is $E = m c^2$ here");

        let on = text_of(&block.to_lines(&p(), ThinkingMode::Visible, None, 80, ImageOpts::off()));
        assert!(on.contains("c²"), "math on renders the grid: {on}");

        let mut off_palette = p();
        off_palette.math_mode = MathMode::Off;
        let off = text_of(&block.to_lines(
            &off_palette,
            ThinkingMode::Visible,
            None,
            80,
            ImageOpts::off(),
        ));
        assert!(off.contains("$E = m c^2$"), "math off stays literal: {off}");
        assert!(!off.contains("c²"), "math off: {off}");
    }

    #[test]
    fn test_thinking_code_block_colors() {
        let mut block = ThinkingBlock::new();
        block.append("like this:\n```\nlet x = 1;\n```");
        let lines = block.to_lines(&p(), ThinkingMode::Visible, None, 80, ImageOpts::off());
        let pairs = span_pairs(&lines);
        // Code block content keeps the accent color, not thinking gray.
        let (_, style) = find_span(&pairs, "let x = 1;");
        assert_eq!(style.fg, Some(p().accent), "code block fg: {style:?}");
    }

    /// The alignment contract: a fenced code block renders exactly like the
    /// same block in assistant content — syntect highlighting, line-number
    /// gutter, borders. Only the surrounding prose is recolored, which is
    /// what keeps a code block *recognizable* inside reasoning.
    #[test]
    fn test_thinking_code_block_matches_content_render() {
        let text = "here is code:\n\n```rust\nlet x = 1;\nlet y = \"two\";\n```\n\ndone";
        let mut block = ThinkingBlock::new();
        block.append(text);
        let thinking = block.to_lines(&p(), ThinkingMode::Visible, None, 80, ImageOpts::off());
        let content = crate::render::markdown::stream::full_lines(
            text,
            80,
            crate::render::markdown::Profile::Content,
            &p(),
        );

        // Compare the code block region only: prose differences are by design
        // (thinking fg), the block itself must be span-identical.
        let block_region = |lines: &[Line<'static>]| -> Vec<(String, Style)> {
            let start = lines
                .iter()
                .position(|l| l.to_string().contains("┌─ rust ─"))
                .expect("top border");
            let end = lines
                .iter()
                .position(|l| l.to_string().contains("└────────"))
                .expect("bottom border");
            lines[start..=end]
                .iter()
                .flat_map(|l| l.spans.iter())
                // The cell prefix / separator blanks carry the thinking fg by
                // design; the block's own tokens are what must match.
                .filter(|s| !s.content.trim().is_empty())
                .map(|s| (s.content.to_string(), s.style))
                .collect()
        };
        assert_eq!(
            block_region(&thinking),
            block_region(&content),
            "reasoning code block must render like assistant content"
        );
        // …and it must really be highlighted, not one flat code color.
        let mut colors: Vec<String> = block_region(&thinking)
            .iter()
            .map(|(_, style)| format!("{:?}", style.fg))
            .collect();
        colors.sort_unstable();
        colors.dedup();
        assert!(
            colors.len() >= 3,
            "expected border + several token colors, got {colors:?}"
        );
    }

    /// Reasoning's own rule: indented (4-space) blocks are nesting, not code.
    #[test]
    fn test_thinking_renders_indented_blocks_as_prose() {
        let mut block = ThinkingBlock::new();
        block.append("a thought:\n\n    a nested **nesting** with `code`\n\nback");
        let lines = block.to_lines(&p(), ThinkingMode::Visible, None, 80, ImageOpts::off());
        let pairs = span_pairs(&lines);
        assert!(
            !pairs
                .iter()
                .any(|(text, _)| text.contains('┌') || text.contains('└')),
            "indented reasoning rendered a code frame: {pairs:?}"
        );
        let (_, nested) = find_span(&pairs, "nested ");
        assert_eq!(nested.fg, Some(p().thinking), "nested prose fg: {nested:?}");
        let (_, bold) = find_span(&pairs, "nesting");
        assert!(
            bold.add_modifier.contains(Modifier::BOLD) && bold.fg == Some(p().thinking),
            "bold keeps its modifier under the thinking fg: {bold:?}"
        );
        let (_, inline) = find_span(&pairs, "code");
        assert_eq!(inline.fg, Some(p().accent), "inline code fg: {inline:?}");
    }

    #[test]
    fn test_thinking_empty() {
        let block = ThinkingBlock::new();
        let lines = block.to_lines(&p(), ThinkingMode::Visible, None, 80, ImageOpts::off());
        // Empty content → header + blank line.
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(line_text(&lines[0]).contains("深度思考"), "{lines:?}");
    }
}
