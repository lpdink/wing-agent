//! 消息里的图片：正文之后的图块，或（画不了时）一行一张的标签。
//!
//! 一条 user 消息的**内容**是 [`UserMessageBody`]：正文 + 媒体引用（`MediaRef`，与线格式
//! 同形，字节永不在这里）。它渲染成什么，取决于这一帧的图片能力：
//!
//! | 状态 | 屏幕上是什么 |
//! |---|---|
//! | `rendering.images: off` / 终端无图形协议 | 标签行 `[Image #1 1024×768 384KB]`（**与引入图块之前逐字节一致**） |
//! | 可画 | 每张图一个**锚点块**：caption 行 `▢ Image #1 · 1024×768` + 覆盖行；图由 app 画上去 |
//! | 这张图不可锚定（id 形态不合法 / 尺寸退化） | 该图退回标签行（与 markdown「没有元数据走链接」同一条阶梯） |
//!
//! **形状来自引用本身**（`MediaRef.width/height`，上传时由网关按 magic bytes 解析），
//! 所以布局是消息内容的纯函数：不探测、不读盘、不等网络。字节到没到只决定「图上不上」
//! （画不出来时 caption 留着），不影响行数——滚动时气泡不会因为图加载而跳动。
//!
//! 号码是**消息内** 1-based 序号（按媒体在消息里的顺序），与草稿的 chip 号码是两套口径：
//! 重放只有媒体列表，没有草稿号码，而消息的标签该由消息自己决定。
//!
//! **已知边界（合并轮次下的号码漂移）**：后端把多份提交合并成一条历史记录时，活体是
//! 「每份提交各自从 1 编号」、重放是「消息内序号」——同一段内容重启前后长得不一样，
//! 纯展示、无数据影响（见 `docs/dev/tui-input.md` 的「消息里的图片」一节），不修。

use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use crate::protocol::MediaRef;
use crate::protocol::is_media_id;
use crate::render::markdown::ComposedLines;
use crate::render::markdown::ImageOpts;
use crate::render::markdown::ImageShape;
use crate::render::markdown::ImageSpan;
use crate::render::markdown::anchor_rows;
use crate::render::markdown::caption_of;
use crate::render::markdown::render_plain;
use crate::render::picture::PictureSource;
use crate::util::format;

/// 气泡的左侧内缩（背景填充与文本区共用这个列数）。
///
/// **唯一声明处**：`viewport` 的文本区、`frame` 的复制内缩表、`cached_cell` 的折行宽度都读它，
/// 改内边距只改这里。
pub(crate) const LEFT_INSET: u16 = 2;

/// 气泡文本区比 cell 窄掉的列数：左内缩 + 右侧留白 1 列。
pub(crate) const TEXT_WIDTH_SUB: u16 = LEFT_INSET + 1;

/// 一条 user 消息的内容：正文 + 它携带的图片引用。
///
/// `media` 与线格式同序同形（`Message.content` / `Message.media` 是两个字段，不承诺交错
/// 顺序——图片排在正文之后）。`From<&str>/From<String>` 让「纯文本消息」说成一句话，
/// 也让绝大多数构造点（测试）不必知道图片的存在。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserMessageBody {
    /// 正文（原样，未 trim：重放要把历史里的字节如实画出来）。
    pub text: String,
    /// 消息携带的图片引用，按消息内顺序。
    pub media: Vec<MediaRef>,
}

impl UserMessageBody {
    /// A message with text and pictures.
    pub fn new(text: impl Into<String>, media: Vec<MediaRef>) -> Self {
        Self {
            text: text.into(),
            media,
        }
    }

    /// 正文与图片都为空（这样的消息不该进视图）。
    pub fn is_empty(&self) -> bool {
        self.text.is_empty() && self.media.is_empty()
    }

    /// 显示文本：正文 + 每张图一行标签。
    ///
    /// 这是**画不了图**时的形态（`images.off` / 无图形协议 / 丢弃态），也是测试与基线比较
    /// 用的文本形态。无 media 时**恒等**返回正文。
    pub fn label_text(&self) -> String {
        text_with_labels(&self.text, &labels_of_media(&self.media))
    }
}

impl From<&str> for UserMessageBody {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            media: Vec::new(),
        }
    }
}

impl From<String> for UserMessageBody {
    fn from(text: String) -> Self {
        Self {
            text,
            media: Vec::new(),
        }
    }
}

/// 消息的显示文本：正文 + 每张图一行标签（正文为空时只有标签行）。
///
/// 图片排在正文**之后**（D5：不承诺文本与图片的交错顺序，后端也是两个字段）。正文已经以
/// `\n` 收尾时不再补一行——带 media 的原始 HTTP / WS 客户端写进历史的尾换行不该在 resume 时
/// 多出一个空行。（TUI 自己的提交路径先 `trim()`，走不到这一支。）
pub fn text_with_labels(content: &str, labels: &[String]) -> String {
    if labels.is_empty() {
        return content.to_string();
    }
    let mut text = String::from(content);
    for label in labels {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(label);
    }
    text
}

/// 一份**尚未上传**的压缩产物在上传后会得到的引用（内容寻址的预测）。
///
/// 粘贴产物恒为 JPEG（见 `util::image_compress`），尺寸是编码后的像素尺寸——正好是网关
/// 上传闸门会写进 `MediaRef` 的那两个数。
pub fn predicted_media(image: &crate::util::image_compress::CompressedImage) -> MediaRef {
    crate::protocol::media::predicted_ref(
        &image.bytes,
        crate::util::image_compress::JPEG_MIME,
        image.width,
        image.height,
    )
}

/// 消息里的标签：媒体引用按消息内顺序编号（尺寸与字节数都取引用里的提示性元数据——
/// 真正发出去的字节在存储里，这里只需要那行文案）。
pub fn labels_of_media(media: &[MediaRef]) -> Vec<String> {
    media
        .iter()
        .enumerate()
        .map(|(index, media)| {
            format::image_label(index + 1, media.width, media.height, media.bytes)
        })
        .collect()
}

/// 一条 user 消息的渲染结果（行 + 图锚点侧通道）。
///
/// `anchors == false` 是「只画标签」的形态：`images.off` 的整个世界，以及被中断丢弃的消息
/// （墓碑态：一张没被划掉的图会让人以为它发出去了）。
pub(super) fn render_body(
    body: &UserMessageBody,
    style: Style,
    width: u16,
    images: &ImageOpts,
    anchors: bool,
) -> ComposedLines {
    if !anchors || !images.is_enabled() {
        return ComposedLines::plain(plain_lines(&body.label_text(), style));
    }
    let text_width = width.saturating_sub(TEXT_WIDTH_SUB);
    let cell = images.cell_pixels();

    let mut lines = plain_lines(&body.text, style);
    let mut spans: Vec<Vec<ImageSpan>> = vec![Vec::new(); lines.len()];
    for (index, media) in body.media.iter().enumerate() {
        let number = index + 1;
        let shape = ImageShape::new(media.width, media.height);
        if !is_media_id(&media.id) || !shape.is_usable() {
            // 不可锚定：这张图保持标签形态（不占图块的行，也就没有空盒子）。
            lines.push(Line::from(Span::styled(
                format::image_label(number, media.width, media.height, media.bytes),
                style,
            )));
            spans.push(Vec::new());
            continue;
        }
        let rows = anchor_rows(text_width, shape, cell);
        let line = lines.len();
        lines.push(Line::from(Span::styled(
            caption_of(&format!("Image #{number}"), shape, text_width),
            style,
        )));
        spans.push(vec![ImageSpan {
            line,
            column: LEFT_INSET,
            cols: text_width,
            rows,
            source: PictureSource::Media(media.id.clone()),
            alt: format!("Image #{number}"),
            px_w: media.width,
            px_h: media.height,
        }]);
        // 覆盖行：图会盖在上面；它们必须带上消息自己的样式，否则会在气泡背景里打出空洞。
        for _ in 1..rows {
            lines.push(Line::from(Span::styled(" ", style)));
            spans.push(Vec::new());
        }
    }
    ComposedLines::with_images(lines, Vec::new(), spans)
}

/// 纯文本按消息样式渲染（`lines()` 语义：末尾换行不多出空行）。
fn plain_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    render_plain(text)
        .into_iter()
        .map(|line| {
            let content = line.to_string();
            Line::from(Span::styled(content, style))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::fit::CellPixels;
    use crate::render::markdown::MAX_ANCHOR_ROWS;
    use crate::util::image_compress::CompressedImage;

    const CELL: CellPixels = CellPixels::new(10, 20);

    fn media(width: u32, height: u32, bytes: u64) -> MediaRef {
        MediaRef {
            id: "a".repeat(64),
            mime: "image/jpeg".into(),
            bytes,
            width,
            height,
            name: None,
        }
    }

    /// Anchor-capable options for a 10×20 cell (a 80-column cell renders 77 columns of text).
    fn anchors() -> ImageOpts {
        ImageOpts::anchor(None, Vec::new(), CELL)
    }

    fn picture(width: u32, height: u32, bytes: usize) -> CompressedImage {
        CompressedImage {
            bytes: vec![0xFF; bytes],
            width,
            height,
        }
    }

    #[test]
    fn the_text_of_a_message_without_pictures_is_the_content_itself() {
        // 红线：无 media 的消息，两种形态都是正文（cell 逐字节不变）。
        let body = UserMessageBody::from("hello\nworld");
        assert_eq!(body.label_text(), "hello\nworld");
        for images in [ImageOpts::off().clone(), anchors()] {
            let composed = render_body(&body, Style::default(), 40, &images, true);
            let text: Vec<String> = composed.lines().iter().map(|l| l.to_string()).collect();
            assert_eq!(text, ["hello", "world"]);
            assert!(!composed.has_images());
        }
    }

    #[test]
    fn labels_land_after_the_text_one_per_line() {
        let labels = labels_of_media(&[media(1024, 768, 393_216), media(8, 8, 7)]);
        assert_eq!(
            text_with_labels("看这两张", &labels),
            "看这两张\n[Image #1 1024×768 384KB]\n[Image #2 8×8 7B]"
        );
        assert_eq!(
            text_with_labels("", &labels),
            "[Image #1 1024×768 384KB]\n[Image #2 8×8 7B]"
        );
        assert!(labels_of_media(&[]).is_empty());
    }

    #[test]
    fn a_label_line_is_the_shape_of_the_composers_chip() {
        assert_eq!(
            labels_of_media(&[media(1024, 768, 393_216)])[0],
            format::image_label(1, 1024, 768, 393_216)
        );
    }

    #[test]
    fn the_label_form_is_what_an_incapable_terminal_draws() {
        let body = UserMessageBody::new("看这张", vec![media(1024, 768, 393_216)]);
        let composed = render_body(&body, Style::default(), 80, ImageOpts::off(), true);
        let text: Vec<String> = composed.lines().iter().map(|l| l.to_string()).collect();
        assert_eq!(text, ["看这张", "[Image #1 1024×768 384KB]"]);
        assert!(!composed.has_images(), "画不了就没有锚点");
    }

    #[test]
    fn the_discarded_form_never_anchors() {
        // 墓碑态只留标签：图块的像素不受 cell 样式影响，画不出「划掉」。
        let body = UserMessageBody::new("打断", vec![media(1024, 768, 393_216)]);
        let composed = render_body(&body, Style::default(), 80, &anchors(), false);
        assert!(!composed.has_images());
        assert_eq!(composed.lines()[1].to_string(), "[Image #1 1024×768 384KB]");
    }

    #[test]
    fn a_drawable_picture_becomes_a_caption_plus_its_own_rows() {
        let body = UserMessageBody::new("看这张", vec![media(1024, 768, 393_216)]);
        let composed = render_body(&body, Style::default(), 80, &anchors(), true);

        let text_width = 80 - TEXT_WIDTH_SUB;
        let rows = anchor_rows(
            text_width,
            ImageShape::new(1024, 768),
            anchors().cell_pixels(),
        );
        assert_eq!(rows, 29, "1024×768 在 77 列（10×20 cell）下的行数");
        assert_eq!(composed.lines().len(), 1 + usize::from(rows));
        assert_eq!(composed.lines()[0].to_string(), "看这张");
        assert_eq!(
            composed.lines()[1].to_string(),
            "▢ Image #1 · 1024×768",
            "caption 是第一行"
        );
        assert_eq!(composed.lines().last().unwrap().to_string(), " ", "覆盖行");

        let spans = composed.images().iter().flatten().collect::<Vec<_>>();
        assert_eq!(spans.len(), 1);
        let span = spans[0];
        assert_eq!(span.line, 1);
        assert_eq!(span.column, LEFT_INSET, "气泡左内缩");
        assert_eq!(span.cols, text_width);
        assert_eq!(span.rows, rows);
        assert_eq!(span.source, PictureSource::Media("a".repeat(64)));
        assert_eq!(span.alt, "Image #1");
        assert_eq!((span.px_w, span.px_h), (1024, 768));
        assert_eq!(
            composed.images().len(),
            composed.lines().len(),
            "侧通道与行对齐"
        );
    }

    #[test]
    fn a_picture_only_message_starts_at_its_caption() {
        let body = UserMessageBody::new("", vec![media(512, 512, 1024)]);
        let composed = render_body(&body, Style::default(), 40, &anchors(), true);
        assert!(
            composed.lines()[0].to_string().starts_with('▢'),
            "纯图消息不先空一行：{:?}",
            composed.lines()[0]
        );
    }

    #[test]
    fn several_pictures_keep_their_order_and_numbering() {
        let body = UserMessageBody::new(
            "三张",
            vec![media(100, 100, 1), media(200, 100, 2), media(100, 200, 3)],
        );
        let composed = render_body(&body, Style::default(), 40, &anchors(), true);
        let captions: Vec<String> = composed
            .lines()
            .iter()
            .map(|line| line.to_string())
            .filter(|line| line.starts_with('▢'))
            .collect();
        assert_eq!(
            captions,
            [
                "▢ Image #1 · 100×100",
                "▢ Image #2 · 200×100",
                "▢ Image #3 · 100×200"
            ]
        );
        let spans: Vec<&ImageSpan> = composed.images().iter().flatten().collect();
        assert_eq!(spans.len(), 3);
        // 每张图的锚点指向自己的 caption 行，块与块之间不重叠。
        for (index, span) in spans.iter().enumerate() {
            if index > 0 {
                let previous = spans[index - 1];
                assert_eq!(
                    previous.line + usize::from(previous.rows),
                    span.line,
                    "上一块的覆盖行刚好到下一块的 caption 之前"
                );
            }
        }
    }

    #[test]
    fn an_unanchorable_picture_keeps_its_label_line() {
        // id 形态不合法（不是 64 位小写 hex）→ 这一张退回标签，不占图块的行。
        let mut bad = media(100, 100, 7);
        bad.id = "not-an-id".into();
        let body = UserMessageBody::new("看", vec![bad, media(100, 100, 8)]);
        let composed = render_body(&body, Style::default(), 40, &anchors(), true);
        let text: Vec<String> = composed.lines().iter().map(|l| l.to_string()).collect();
        assert_eq!(text[0], "看");
        assert_eq!(text[1], "[Image #1 100×100 7B]", "坏引用只花一行标签");
        assert_eq!(
            text[2], "▢ Image #2 · 100×100",
            "好引用照常成块（号码不乱）"
        );
        let spans = composed.images().iter().flatten().collect::<Vec<_>>();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].line, 2);
    }

    #[test]
    fn a_degenerate_shape_is_not_anchored() {
        let body = UserMessageBody::new("看", vec![media(0, 0, 7)]);
        let composed = render_body(&body, Style::default(), 40, &anchors(), true);
        assert!(!composed.has_images(), "0×0 的引用没有可算的行数");
        assert_eq!(composed.lines()[1].to_string(), "[Image #1 0×0 7B]");
    }

    #[test]
    fn the_block_is_capped_by_the_shared_anchor_rule() {
        let body = UserMessageBody::new("", vec![media(100, 100_000, 7)]);
        let composed = render_body(&body, Style::default(), 40, &anchors(), true);
        assert_eq!(
            composed.lines().len(),
            usize::from(MAX_ANCHOR_ROWS),
            "长图顶到封顶行数，盒子由共享 fit 决定"
        );
    }

    #[test]
    fn a_degenerate_cell_falls_back_to_the_label_form() {
        // `ImageOpts::anchor` 对退化 cell 回答 `off`：没有行数可算，就不出锚点。
        let degenerate = ImageOpts::anchor(None, Vec::new(), CellPixels::new(0, 0));
        let body = UserMessageBody::new("看", vec![media(100, 100, 7)]);
        let composed = render_body(&body, Style::default(), 40, &degenerate, true);
        assert!(!composed.has_images());
    }

    #[test]
    fn the_predicted_ref_is_the_content_addressed_one() {
        let image = picture(640, 480, 4096);
        let predicted = predicted_media(&image);
        assert_eq!(predicted.id, crate::protocol::media_id_of(&image.bytes));
        assert_eq!(predicted.mime, crate::util::image_compress::JPEG_MIME);
        assert_eq!((predicted.width, predicted.height), (640, 480));
        assert_eq!(predicted.bytes, 4096);
    }
}
