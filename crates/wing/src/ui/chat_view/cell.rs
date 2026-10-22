//! The chat view's content unit: [`ChatCell`] and its rendering.
//!
//! A cell owns **what it looks like** (its `to_lines` / `render_lines` output)
//! and nothing else: no scroll state, no viewport geometry, no frame state.
//! Height and line caching live one layer up, in
//! [`CachedCell`](crate::ui::cached_cell::CachedCell). A cell may wrap inside
//! its own output (DiffView hangs an over-wide row's continuation under its
//! gutter); what stays the widget's job (`super::viewport`) is the layout's
//! `Paragraph` wrap for whatever a cell leaves over-wide, and the screen-row
//! arithmetic that follows from it.
//!
//! [`PendingMessage`] lives here too: it is the cell wrapper of a
//! sent-but-not-yet-accepted user message, i.e. another content state, not a
//! viewport concern.

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::config::ThemePalette;
use crate::render::markdown::ComposedLines;
use crate::render::markdown::ImageOpts;
use crate::render::markdown::Profile;
use crate::render::markdown::RenderOpts;
use crate::render::markdown::compose_lines;
use crate::render::markdown::links::CELL_PREFIX_WIDTH;
use crate::render::markdown::render_markdown_lines_with;
use crate::render::renderable::CellContext;
use crate::shared::panels::picker::ModelPanel;
use crate::ui::cached_cell::CachedCell;
use crate::ui::cells::ask_msg::AskMessage;
use crate::ui::cells::diff_view::DiffView;
use crate::ui::cells::model_picker::model_picker_lines;
use crate::ui::cells::notice::NoticeLevel;
use crate::ui::cells::notice::notice_lines;
use crate::ui::cells::thinking::ThinkingBlock;
use crate::ui::cells::todo_msg::TodoMessage;
use crate::ui::cells::tool_call::ToolCallBlock;
use crate::ui::chat_view::media;
use crate::ui::chat_view::media::UserMessageBody;

/// A single cell in the chat view.
///
/// `large_enum_variant` 被显式放行：`ToolCallBlock`（352 B）比第二大的 `DiffView`
/// （128 B）大 200 B 以上，而两个尺寸都不由本文件决定。触发点与 `wing acp` 引入的
/// `agent-client-protocol` 依赖有关——它开启 `serde_json/preserve_order`，Cargo
/// 的特性合并会把该特性并到本 crate 的 `serde_json` 上，`serde_json::Value`
/// （`IndexMap` 化）随之**从 32 B 变成 72 B（+40 B，实测）**，把原本压在线下的差值
/// 顶过了阈值。装箱整个 `ToolCall` 变体是另一条路，但那要改掉所有构造点；这里的放行
/// 只为把 lint 恢复到引入 ACP 依赖之前的基线。
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum ChatCell {
    /// User message: text plus the pictures it carries (`media`), rendered as an anchor
    /// block per picture when the terminal can draw them (see `media`).
    UserMessage(UserMessageBody),
    /// User message awaiting model acceptance — rendered in the pending
    /// area below all committed cells, dimmed to signal "queued, not yet
    /// fed to the model". Promoted to `UserMessage` on
    /// `user_message_accepted`, or demoted to `DiscardedUserMessage` when
    /// an interrupt discards it.
    PendingUserMessage(UserMessageBody),
    /// User message discarded by an interrupt before reaching the model —
    /// dimmed + struck through to signal "never sent".
    DiscardedUserMessage(UserMessageBody),
    /// Assistant text (may be accumulated from streaming TextEvents).
    AssistantMessage(String),
    /// System message.
    SystemMessage(String),
    /// Warning message — a `notice` event rendered as a yellow system
    /// message. Unlike `ErrorMessage` it does **not** imply the turn ended.
    WarningMessage(String),
    /// Error message.
    ErrorMessage(String),
    /// Reasoning/thinking block — 标题行（折叠时是全部可见内容、展开时是
    /// disclosure 头）+ 正文（按展开态）；由 `rendering.thinking` 的默认 +
    /// Ctrl+O 的全局覆盖解析。
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
}

impl ChatCell {
    /// 用户消息气泡（正常 / 排队 / 被丢弃）：文本画在带内缩的文本区里、背景整块填充，
    /// 因此不走 blit 快路（理由见 [`CachedCell::is_prewrapped`]）。
    pub(crate) fn is_user_bubble(&self) -> bool {
        matches!(
            self,
            Self::UserMessage(_) | Self::PendingUserMessage(_) | Self::DiscardedUserMessage(_)
        )
    }

    /// Render this cell to lines **plus their markdown link spans**.
    ///
    /// Cells without markdown links (everything but assistant messages and
    /// thinking blocks) return [`ComposedLines::plain`] — the spans exist so
    /// the widget can inject OSC8 hyperlinks and hit-test clicks without ever
    /// re-deriving what the renderer produced (see `tui-link-open`).
    pub fn render_lines(&self, width: u16, ctx: &CellContext<'_>) -> ComposedLines {
        match self {
            Self::UserMessage(body) => {
                let style = Style::default()
                    .bg(ctx.palette.surface)
                    .fg(ctx.palette.text);
                media::render_body(body, style, width, ctx.images, true)
            }
            Self::PendingUserMessage(body) => {
                let style = Style::default().bg(ctx.palette.surface).fg(ctx.palette.dim);
                media::render_body(body, style, width, ctx.images, true)
            }
            Self::DiscardedUserMessage(body) => {
                let style = Style::default()
                    .bg(ctx.palette.surface)
                    .fg(ctx.palette.dim)
                    .add_modifier(Modifier::CROSSED_OUT);
                media::render_body(body, style, width, ctx.images, false)
            }
            Self::AssistantMessage(text) => {
                assistant_message_lines(text, width, ctx.palette, ctx.images)
            }
            Self::Thinking(block) => block.render_lines(
                ctx.palette,
                ctx.thinking_mode,
                ctx.thinking_expanded,
                width,
                ctx.images,
            ),
            _ => ComposedLines::plain(self.to_lines(width, ctx)),
        }
    }

    /// Render this cell to lines, width-aware for full-width elements.
    pub fn to_lines(&self, width: u16, ctx: &CellContext<'_>) -> Vec<Line<'static>> {
        let palette = ctx.palette;
        match self {
            Self::UserMessage(body) => {
                let style = Style::default().bg(palette.surface).fg(palette.text);
                media::render_body(body, style, width, ctx.images, true).into_lines()
            }
            Self::PendingUserMessage(body) => {
                // Queued, not yet accepted by the model — dimmed.
                let style = Style::default().bg(palette.surface).fg(palette.dim);
                media::render_body(body, style, width, ctx.images, true).into_lines()
            }
            Self::DiscardedUserMessage(body) => {
                // Discarded by an interrupt before reaching the model —
                // dimmed + struck through. 只留标签：一张没被划掉的图会让人以为
                // 它发出去了（图块的像素不受 cell 样式影响，画不出「划掉」）。
                let style = Style::default()
                    .bg(palette.surface)
                    .fg(palette.dim)
                    .add_modifier(Modifier::CROSSED_OUT);
                media::render_body(body, style, width, ctx.images, false).into_lines()
            }
            Self::AssistantMessage(text) => {
                assistant_message_lines(text, width, palette, ctx.images).into_lines()
            }
            Self::SystemMessage(text) => notice_lines(text, NoticeLevel::Info, width, palette),
            Self::WarningMessage(text) => {
                // A notice (e.g. "retrying in 6s") — yellow, but explicitly not
                // an error: the turn is still running.
                notice_lines(text, NoticeLevel::Warning, width, palette)
            }
            Self::ErrorMessage(text) => notice_lines(text, NoticeLevel::Error, width, palette),
            Self::Thinking(block) => block.to_lines(
                palette,
                ctx.thinking_mode,
                ctx.thinking_expanded,
                width,
                ctx.images,
            ),
            Self::ToolCall(block) => block.to_lines(palette, ctx.layout.tool_output_max),
            Self::Diff(view) => view.to_lines(palette, width),
            Self::Todo(msg) => msg.to_lines(palette),
            Self::Ask(msg) => msg.to_lines(palette, width),
            Self::ModelPicker(panel) => model_picker_lines(panel, width, palette),
            Self::Separator => {
                let sep = "─".repeat(width as usize);
                vec![Line::from(Span::styled(
                    sep,
                    Style::default().fg(palette.dim),
                ))]
            }
        }
    }
}

/// Assistant message lines: markdown prefixed with `⦁ ` / `  `, plus the link
/// spans of every rendered line (shifted by the 2-column prefix).
///
/// Reserves 2 columns for the prefix so tables balance to fit and downstream
/// wrapping never breaks a row. `images` are the frame's image options: the
/// anchor rows a standalone picture reserves are laid out here, with the same
/// options the streaming engine holds (see `CachedCell::set_image_opts`).
fn assistant_message_lines(
    text: &str,
    width: u16,
    palette: &ThemePalette,
    images: &ImageOpts,
) -> ComposedLines {
    let md_width = Some(width.saturating_sub(2));
    let md_lines = render_markdown_lines_with(
        text,
        md_width,
        palette,
        // The math mode travels with the palette (L1) and the image options
        // with the frame (L4) — the content cell honours both.
        RenderOpts::new(Profile::Content, true)
            .with_math(palette.math_mode)
            .with_images(images),
    );
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

/// A user message that has been sent but not yet accepted by the model.
///
/// Rendered in the pending area below all committed cells ("stuck at the
/// bottom") until `user_message_accepted` promotes it into history.
pub struct PendingMessage {
    /// ClientRequest.request_id — correlates with UserMessageAccepted.
    pub request_id: String,
    /// 提交产物（正文 + 图片）：气泡不读它——它留着给**失败回填**（发送失败 / 服务端
    /// 拒绝时把内容原样还给草稿）。生命周期与条目本身绑定，没有第二处需要清理。
    pub(crate) submission: Submission,
    pub(crate) cell: CachedCell, // PendingUserMessage(display text)
}

/// 一次提交的产物（`SubmitPayload` 去掉展示那一半后的**原件**）：正文 + 图片。
///
/// 正文是消息文本本身（图片 chip 不贡献字符）；图片按提交顺序。显示文本由
/// [`crate::ui::chat_view::media`] 现拼（正文 + 每张图一行标签），两者分开存是因为
/// 回填草稿时标签行**不能再进正文**（chip 会自己重建标签）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    pub content: String,
    pub images: Vec<crate::util::image_compress::CompressedImage>,
    /// 上传之后会就绪的媒体引用（本机按内容寻址预测，与 `images` 同序同长）。
    ///
    /// 预测而不是等回执，是为了让 pending 气泡**立刻**有图（字节就在 `images` 里）：
    /// 端点的 id 恒为字节的 sha256，所以预测与响应逐字节一致（`protocol::media`）。
    /// 回填草稿只读 `content` / `images`，这些引用不参与。
    ///
    /// 私有：唯一合法的来源是 [`Submission::new`]（派生自 `images`，两者等长）——
    /// 绕过它构造出「有图无引用」会让气泡静默丢掉图。
    media: Vec<crate::protocol::MediaRef>,
}

impl Submission {
    /// 一份纯文本产物（没有图片的提交）。
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            images: Vec::new(),
            media: Vec::new(),
        }
    }

    /// 正文 + 图片的提交产物：媒体引用按内容寻址预测（见 [`Submission::media`]）。
    pub fn new(
        content: impl Into<String>,
        images: Vec<crate::util::image_compress::CompressedImage>,
    ) -> Self {
        let media = images.iter().map(media::predicted_media).collect();
        Self {
            content: content.into(),
            images,
            media,
        }
    }

    /// 这条消息的媒体引用（与 [`Submission::images`] 同序同长）。
    pub fn media(&self) -> &[crate::protocol::MediaRef] {
        &self.media
    }

    /// 这条消息在气泡里的内容（正文 + 图片引用）。
    pub fn body(&self) -> UserMessageBody {
        UserMessageBody::new(self.content.clone(), self.media.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{make_ctx, test_ctx};
    use super::*;

    #[test]
    fn test_user_message_height() {
        let cell = ChatCell::UserMessage("hello".into());
        let (p, l) = test_ctx();
        let ctx = make_ctx(&p, &l);
        let mut cached = crate::ui::cached_cell::CachedCell::new(cell);
        let h = cached.compute_height(80, &ctx);
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

    /// The assistant-message cell reads the math mode off the palette, like
    /// every other render path: `off` must leave the LaTeX source verbatim
    /// here too (the cell builds its own `RenderOpts`, so a hardcoded mode
    /// would silently diverge from the streaming engine and the thinking
    /// cell).
    #[test]
    fn test_assistant_message_follows_the_palette_math_mode() {
        use crate::config::rendering::MathMode;

        let (mut palette, layout) = test_ctx();
        let md = "energy is $E = m c^2$ here";
        let render = |palette: &ThemePalette| {
            let ctx = make_ctx(palette, &layout);
            ChatCell::AssistantMessage(md.into())
                .to_lines(80, &ctx)
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };

        let on = render(&palette);
        assert!(on.contains("c²"), "math on renders the grid: {on}");
        assert!(!on.contains("$E = m c^2$"), "math on: {on}");

        palette.math_mode = MathMode::Off;
        let off = render(&palette);
        assert!(off.contains("$E = m c^2$"), "math off stays literal: {off}");
        assert!(!off.contains("c²"), "math off: {off}");
    }
}
