//! App 侧的粘贴通道边界：两条读取通道（OSC 5522 的 `TermEvent::Clipboard`，本地
//! 读取的 `FetchPayload::ClipboardPaste`）在这里汇进同一个落点。
//!
//! 文本与 bracketed paste 走**同一个**入口（[`App::handle_paste`]），语义逐点等价；
//! 图片走一条**单并发流水线**：入队（原始字节，含上限）→ 后台压缩 → 插入 composer 的
//! 图片 chip（见 `ui/input_area/image.rs`）。失败一律降级为 toast，绝不动用户已有内容。
//!
//! 「一次一张」不是保守，而是**顺序契约**：两张并发压完的到达序不确定，chip 的号码
//! （= 插入序）就会跳序。出队即消费（队列里只留还没开始压的那些），所以 04 的暂存区
//! 在这里升级为流水线，而不是被丢掉。
//!
//! 这一层刻意薄：协议 / 通道 / 超时全在 [`crate::tui::clipboard_protocol`]，压缩在
//! `util/image_compress`，chip 在 composer；这里只有「读回的字节怎么变成 chip」这一件事。

use std::time::Duration;

/// 队列的上限：**条数**与**总字节**。
///
/// 队列只在「压缩还没轮到这一张」时持有原始字节（单张最大 64 MiB，
/// `clipboard_protocol::MAX_PAYLOAD`），到顶时丢掉**最旧**的一张（FIFO）并记日志：
/// 新的一次粘贴是用户的当下意图，优先保住。已经出了队的那些字节在压缩任务手里，不占这份
/// 预算——所以这个上限管的是「还排着队的原始字节」，不是草稿里 chip 的张数（chip 的图片
/// 已按 `paste.image.max_bytes` 压过，且可删；粘贴文本的 chip 同样没有张数上限）。
pub(crate) const MAX_PENDING_IMAGES: usize = 8;
pub(crate) const MAX_PENDING_IMAGE_BYTES: usize = 64 * 1024 * 1024;

use crate::app::intent::AppIntent;
use crate::tui::ClipboardFailure;
use crate::tui::ClipboardPayload;
use crate::ui::toast::Toast;
use crate::util::clipboard_paste::PasteFailure;
use crate::util::clipboard_paste::PasteOutcome;
use crate::util::image_compress::CompressedImage;

use super::App;

/// 终端协议通道读回、还没有变成 chip 的图片：流水线的队列元素。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingClipboardImage {
    /// 原始字节（未压缩；压缩在出队之后做）。
    pub bytes: Vec<u8>,
    /// 终端给出的 MIME（如 `image/png`）——只用于日志与失败文案，压缩器自己嗅探格式。
    pub mime: String,
    /// 文件名——OSC 5522 不带这个信息，恒为 `None`。
    pub name: Option<String>,
}

impl App {
    /// `TermEvent::Clipboard` 的分派：文本 → composer（或当前模态接受者），图片 → 流水线，
    /// 失败 → toast。
    pub(crate) fn handle_clipboard(&mut self, result: Result<ClipboardPayload, ClipboardFailure>) {
        match result {
            Ok(ClipboardPayload::Text(text)) => self.handle_paste(&text),
            Ok(ClipboardPayload::Image { bytes, mime, name }) => {
                self.on_clipboard_image(bytes, mime, name)
            }
            Err(failure) => self.on_clipboard_failure(failure),
        }
    }

    /// 本地剪贴板读取（L1，`Ctrl+V` / `super+v`）的回执落点：文本 → 与 bracketed
    /// paste 同一个入口（模态挡板自动继承）；图片 → 同一条单并发流水线；空 / 失败 →
    /// 定向 toast（与 OSC 5522 通道同一条「不静默」红线）。
    pub(crate) fn on_clipboard_paste(&mut self, result: Result<PasteOutcome, PasteFailure>) {
        match result {
            Ok(PasteOutcome::Text(text)) => self.handle_paste(&text),
            // 本地读取的通道不带文件名（OSC 5522 也不带）。
            Ok(PasteOutcome::Image { bytes, mime }) => self.on_clipboard_image(bytes, mime, None),
            Ok(PasteOutcome::Empty) => {
                self.show_toast(Toast::warning(
                    "Clipboard has no text or image",
                    Duration::from_secs(4),
                ));
            }
            Err(failure) => {
                tracing::warn!(failure = %failure, "local clipboard read failed");
                self.show_toast(Toast::warning(failure.message(), Duration::from_secs(4)));
            }
        }
    }

    /// 图片载荷边界：收下字节并把它交给压缩流水线。
    ///
    /// **不查模态挡板**（与文本不同）：文本粘贴是往某个输入框里送字，模态面板该拦；
    /// 图片落的是草稿上的一个可见、可删的 chip，静默丢字节才是最坏的结果。
    pub(crate) fn on_clipboard_image(
        &mut self,
        bytes: Vec<u8>,
        mime: String,
        name: Option<String>,
    ) {
        tracing::info!(
            bytes = bytes.len(),
            mime = %mime,
            "clipboard image queued for compression"
        );
        self.pending_clipboard_images
            .push(PendingClipboardImage { bytes, mime, name });
        self.trim_pending_images();
        self.pump_clipboard_images();
    }

    /// 出一张交给压缩器（一次一张）。
    ///
    /// 没有在飞的压缩时取队首：字节 **move** 进 intent（64 MiB 级的克隆不可接受），
    /// 队列里因此只剩还没轮到的那些。「在飞」标记是这里唯一的并发门——它同时保证了
    /// **插入序 = 粘贴序**。
    fn pump_clipboard_images(&mut self) {
        if self.clipboard_compress_in_flight || self.pending_clipboard_images.is_empty() {
            return;
        }
        let image = self.pending_clipboard_images.remove(0);
        tracing::debug!(
            bytes = image.bytes.len(),
            mime = %image.mime,
            name = ?image.name,
            "compressing a pasted image"
        );
        self.clipboard_compress_in_flight = true;
        self.push_intent(AppIntent::CompressClipboardImage {
            bytes: image.bytes,
            options: self.config.paste.image.compress_options(),
        });
    }

    /// 一张压完了：插 chip（失败则 toast），然后看队列里还有没有下一张。
    ///
    /// 插入点取**此刻**的光标，不是粘贴时刻的（压缩异步，原位插入要假定那时的行列仍
    /// 有效）——见 `docs/dev/tui-input.md` 的「已知边界（图片 chip 的落点）」。
    ///
    /// 会话无关：哪怕这张图是在另一个会话里粘的，草稿还在，chip 就还得插（所以
    /// `handle_fetch_result` 对这条回执不查会话守卫）。
    pub(crate) fn on_clipboard_compressed(&mut self, result: Result<CompressedImage, String>) {
        self.clipboard_compress_in_flight = false;
        match result {
            Ok(image) => {
                let number = self.input.insert_image(image);
                tracing::info!(chip = number, "pasted image inserted as a chip");
            }
            Err(reason) => {
                tracing::warn!(reason = %reason, "pasted image could not be compressed");
                self.show_toast(Toast::warning(
                    format!("Image dropped: {reason}"),
                    Duration::from_secs(5),
                ));
            }
        }
        self.pump_clipboard_images();
    }

    /// 通道失败（权限 / 超时 / 坏数据）：toast + 日志，别的一个都不动。
    pub(crate) fn on_clipboard_failure(&mut self, failure: ClipboardFailure) {
        tracing::warn!(failure = ?failure, "clipboard read failed");
        self.show_toast(Toast::warning(failure.message(), Duration::from_secs(4)));
    }

    /// 队列里还没开始压的图片（测试的观察口；张数已含「正在压缩的那一张不在队列里」）。
    pub fn pending_clipboard_images(&self) -> &[PendingClipboardImage] {
        &self.pending_clipboard_images
    }

    /// 把队列收进上限（FIFO 丢最旧的，见 [`MAX_PENDING_IMAGES`]）。
    ///
    /// 丢弃要出声（与协议机队列满同一条红线）：静默丢图是「粘了没反应」的最坏形态，
    /// 所以这里和别的失败一样走 warning toast。
    fn trim_pending_images(&mut self) {
        let mut total: usize = self
            .pending_clipboard_images
            .iter()
            .map(|i| i.bytes.len())
            .sum();
        let mut dropped = 0usize;
        while self.pending_clipboard_images.len() > MAX_PENDING_IMAGES
            || total > MAX_PENDING_IMAGE_BYTES
        {
            if self.pending_clipboard_images.len() == 1 {
                // 最后一张再大也留着：它就是用户刚粘的那一张。
                break;
            }
            let oldest = self.pending_clipboard_images.remove(0);
            total -= oldest.bytes.len();
            dropped += 1;
            tracing::warn!(
                bytes = oldest.bytes.len(),
                mime = %oldest.mime,
                "clipboard image queue over budget, dropped the oldest"
            );
        }
        if dropped > 0 {
            self.show_toast(Toast::warning(
                "Image dropped: too many pending images",
                Duration::from_secs(4),
            ));
        }
    }
}
