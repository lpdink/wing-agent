//! App 侧的粘贴通道边界：两条读取通道（OSC 5522 的 `TermEvent::Clipboard`，本地
//! 读取的 `FetchPayload::ClipboardPaste`）在这里汇进同一个落点。
//!
//! 文本与 bracketed paste 走**同一个**入口（[`App::handle_paste`]），语义逐点等价；
//! 图片走一条**单并发流水线**：占位符 chip 先落地（粘贴那一刻）→ 字节到齐 → 后台压缩
//! → 原地翻成正式 chip（见 `ui/input_area/image.rs`）。失败一律降级为 toast + 原地
//! 的 `✗`，绝不动用户已有内容。
//!
//! **占位符是这条流水线的锚**。字节要经终端传、压缩要在后台跑，两者加起来可以是一秒
//! 级；在图片「已经贴上去了」这件事上，用户的体感只有占位符给得了。所以：
//!
//! - 5522 通道在**读开始**时就插一个（协议机选定了图片 MIME，字节还没到）；
//! - 本地读取通道读回字节才知道是图片，占位符在那一刻插（`Compressing`）；
//! - 进度（节流后的累计字节数）与阶段变化只改这一个 chip 的标签，位置不动；
//! - 尺寸与号码在插入时就定了 —— 号码 = 粘贴序，落定只是换个标签。
//!
//! 「一次一张」不是保守，而是**顺序契约**：两张并发压完的到达序不确定，chip 的号码
//! （= 插入序）就会跳序。出队即消费（队列里只留还没开始压的那些），所以 04 的暂存区
//! 在这里升级为流水线，而不是被丢掉。
//!
//! 这一层刻意薄：协议 / 通道 / 超时全在 [`crate::tui::clipboard_protocol`]，压缩在
//! `util/image_compress`，chip 在 composer；这里只有「读回来 / 正在读的字节怎么变成
//! chip」这一件事。

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
use crate::tui::ClipboardEvent;
use crate::tui::ClipboardFailure;
use crate::tui::ClipboardPayload;
use crate::ui::input_area::ImageChipTarget;
use crate::ui::input_area::ImageStage;
use crate::ui::toast::Toast;
use crate::util::clipboard_paste::PasteFailure;
use crate::util::clipboard_paste::PasteOutcome;
use crate::util::image_compress::CompressedImage;

use super::App;

/// 终端协议通道读回、还没有变成 chip 的图片：流水线的队列元素。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingClipboardImage {
    /// 这张图在草稿里的占位符（字节到手时它已经在了）。
    pub target: ImageChipTarget,
    /// 原始字节（未压缩；压缩在出队之后做）。
    pub bytes: Vec<u8>,
    /// 终端给出的 MIME（如 `image/png`）——只用于日志与失败文案，压缩器自己嗅探格式。
    pub mime: String,
    /// 文件名——OSC 5522 不带这个信息，恒为 `None`。
    pub name: Option<String>,
}

impl App {
    /// `TermEvent::Clipboard` 的分派：读的生命周期五态（见 [`ClipboardEvent`]）。
    pub(crate) fn handle_clipboard(&mut self, event: ClipboardEvent) {
        match event {
            ClipboardEvent::ImageReadStarted { mime } => self.on_clipboard_image_started(mime),
            ClipboardEvent::ReadProgress { received } => self.on_clipboard_image_progress(received),
            ClipboardEvent::Delivered(payload) => match payload {
                ClipboardPayload::Text(text) => self.handle_paste(&text),
                ClipboardPayload::Image { bytes, mime, name } => {
                    self.on_clipboard_image_delivered(bytes, mime, name)
                }
            },
            // 在途读的失败：占位符就地翻成 `✗`（**只在它还在的时候**：用户可能已经
            // 把占位符删了 —— 那正是「这张我不要了」的动作）。只有图片读有占位符，
            // 所以 MIME 决定要不要动草稿。
            ClipboardEvent::ReadFailed { mime, failure } => {
                self.on_clipboard_read_failed(&mime, failure)
            }
            // 与在读的那条无关的丢弃：没有占位符可翻，别误伤正在传的那张图。
            ClipboardEvent::PasteRejected(failure) => self.on_clipboard_rejected(failure),
        }
    }

    /// 本地剪贴板读取（L1，`Ctrl+V` / `super+v`）的回执落点：文本 → 与 bracketed
    /// paste 同一个入口（模态挡板自动继承）；图片 → 同一条单并发流水线；空 / 失败 →
    /// 定向 toast（与 OSC 5522 通道同一条「不静默」红线）。
    pub(crate) fn on_clipboard_paste(&mut self, result: Result<PasteOutcome, PasteFailure>) {
        match result {
            Ok(PasteOutcome::Text(text)) => self.handle_paste(&text),
            // 本地读取的通道不带文件名（OSC 5522 也不带），也**没有「读开始」信号**：
            // 占位符只能在此刻插（见 `on_clipboard_local_image`）。
            Ok(PasteOutcome::Image { bytes, mime }) => self.on_clipboard_local_image(bytes, mime),
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

    /// 5522 通道：一条图片读开始了（协议机已选定图片 MIME，字节还没到）。
    ///
    /// 占位符就在这一刻落地 —— 这是「粘贴已受理」对用户唯一的证据，也是整个特性的
    /// 起点（见 [`Self::on_clipboard_local_image`] 对「本地通道没这一步」的说明）。
    /// 句柄入队，与随后的交付 / 失败配对（见 `App::image_reads`）。
    fn on_clipboard_image_started(&mut self, mime: String) {
        let target = self
            .input
            .insert_pending_image(ImageStage::Receiving { bytes: 0 });
        tracing::info!(
            chip = target.number,
            mime = %mime,
            "clipboard image read started"
        );
        self.image_reads.push_back(target);
    }

    /// 5522 通道：在途读的进度（协议机节流后的**累计**字节数）。
    ///
    /// 只改标签：不碰 chat、不重排草稿，位置与号码都不动。
    fn on_clipboard_image_progress(&mut self, received: u64) {
        let Some(target) = self.image_reads.front().copied() else {
            return;
        };
        if let Some(shift) = self
            .input
            .set_image_stage(target, ImageStage::Receiving { bytes: received })
        {
            self.selection.shift_composer_selection(shift);
        }
    }

    /// 5522 通道：一条图片读的**字节到手** —— 认领它在开始时登记的占位符，
    /// 把它翻成「处理中」并交给压缩流水线。
    ///
    /// 认领是 `image_reads` 队首，且**只属于这条通道**（每条 5522 读都有对应的
    /// `ImageReadStarted`）。本地通道绝不进这里：认领会把一张本地图片塞进正在传的
    /// 那张图的占位符里（号码 / 位置全错，而那张 5522 图反倒在字节到时新建 chip）。
    ///
    /// 占位符已经不在了（草稿被清空 / 重写、chip 被删）⇒ 字节直接丢掉：那次粘贴在
    /// 用户眼里早已作废，重新插一个 chip 才是意外（删占位符 = 「这张我不要了」）。
    fn on_clipboard_image_delivered(&mut self, bytes: Vec<u8>, mime: String, name: Option<String>) {
        let Some(target) = self.image_reads.pop_front() else {
            // 没有在途读却收到图片字节（不该发生）：按本地通道处理，至少让字节落地。
            tracing::debug!(mime = %mime, "clipboard image arrived with no read in flight");
            self.on_clipboard_local_image(bytes, mime);
            return;
        };
        if !self.input.image_target_is_live(target) {
            tracing::debug!(
                chip = target.number,
                "pasted image dropped: its placeholder is gone"
            );
            return;
        }
        if let Some(shift) = self.input.set_image_stage(target, ImageStage::Compressing) {
            self.selection.shift_composer_selection(shift);
        }
        self.enqueue_clipboard_image(target, bytes, mime, name);
    }

    /// 本地通道（L1）：读回字节才知道是图片 —— **永远新插**一个占位符，此刻进入
    /// 「处理中」。
    ///
    /// **不查模态挡板**（与文本不同）：文本粘贴是往某个输入框里送字，模态面板该拦；
    /// 图片落的是草稿上一个可见、可删的 chip，静默丢字节才是最坏的结果。
    pub(crate) fn on_clipboard_local_image(&mut self, bytes: Vec<u8>, mime: String) {
        let target = self.input.insert_pending_image(ImageStage::Compressing);
        self.enqueue_clipboard_image(target, bytes, mime, None);
    }

    /// 图片载荷的入队尾巴（两条通道共用）：日志 → 队列 → 上限 → 泵。
    fn enqueue_clipboard_image(
        &mut self,
        target: ImageChipTarget,
        bytes: Vec<u8>,
        mime: String,
        name: Option<String>,
    ) {
        tracing::info!(
            bytes = bytes.len(),
            mime = %mime,
            chip = target.number,
            "clipboard image queued for compression"
        );
        self.pending_clipboard_images.push(PendingClipboardImage {
            target,
            bytes,
            mime,
            name,
        });
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
            chip = image.target.number,
            "compressing a pasted image"
        );
        self.clipboard_compress_in_flight = true;
        self.push_intent(AppIntent::CompressClipboardImage {
            target: image.target,
            bytes: image.bytes,
            options: self.config.paste.image.compress_options(),
        });
    }

    /// 一张压完了：把它的占位符**原地**翻成正式 chip（失败则 `✗` + toast），
    /// 然后看队列里还有没有下一张。
    ///
    /// 句柄作废（草稿被清空 / 重写、chip 被删）时结果直接丢弃：那张图已经不在任何人的
    /// 视野里了，插回去才是意外。
    ///
    /// 会话无关：哪怕这张图是在另一个会话里粘的，草稿还在，chip 就还得落地（所以
    /// `handle_fetch_result` 对这条回执不查会话守卫）。
    pub(crate) fn on_clipboard_compressed(
        &mut self,
        target: ImageChipTarget,
        result: Result<CompressedImage, String>,
    ) {
        self.clipboard_compress_in_flight = false;
        match result {
            Ok(image) => match self.input.complete_image(target, image) {
                Some(shift) => {
                    self.selection.shift_composer_selection(shift);
                    tracing::info!(chip = target.number, "pasted image chip settled");
                }
                None => {
                    tracing::debug!(
                        chip = target.number,
                        "pasted image dropped: the chip is gone"
                    );
                }
            },
            Err(reason) => {
                tracing::warn!(reason = %reason, "pasted image could not be compressed");
                if let Some(shift) = self.input.fail_image(target) {
                    self.selection.shift_composer_selection(shift);
                }
                self.show_toast(Toast::warning(
                    format!("Image dropped: {reason}"),
                    Duration::from_secs(5),
                ));
            }
        }
        self.pump_clipboard_images();
    }

    /// 在途**读**的失败（超时 / 超限 / 终端拒绝 / 坏包）：占位符翻 `✗` + toast。
    ///
    /// 只有 `image/*` 的读有占位符（文本读没有 Started，也就没有 chip）：文本读的失败
    /// 只 toast，绝不去动队列里的句柄。
    pub(crate) fn on_clipboard_read_failed(&mut self, mime: &str, failure: ClipboardFailure) {
        if mime.starts_with("image/")
            && let Some(target) = self.image_reads.pop_front()
            && let Some(shift) = self.input.fail_image(target)
        {
            self.selection.shift_composer_selection(shift);
        }
        tracing::warn!(mime, failure = ?failure, "clipboard read failed");
        self.show_toast(Toast::warning(failure.message(), Duration::from_secs(4)));
    }

    /// 没有读、也不属于某条读的失败（粘贴被丢弃 / 没有可读表示）：只有 toast。
    pub(crate) fn on_clipboard_rejected(&mut self, failure: ClipboardFailure) {
        tracing::warn!(failure = ?failure, "clipboard paste rejected");
        self.show_toast(Toast::warning(failure.message(), Duration::from_secs(4)));
    }

    /// 队列里还没开始压的图片（测试的观察口；张数已含「正在压缩的那一张不在队列里」）。
    pub fn pending_clipboard_images(&self) -> &[PendingClipboardImage] {
        &self.pending_clipboard_images
    }

    /// 把队列收进上限（FIFO 丢最旧的，见 [`MAX_PENDING_IMAGES`]）。
    ///
    /// 丢弃要出声（与协议机队列满同一条红线）：静默丢图是「粘了没反应」的最坏形态，
    /// 所以这里和别的失败一样走 warning toast —— 被丢的那张图的占位符同时翻成 `✗`
    /// （它已经在用户眼前，不能凭空消失）。
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
            if let Some(shift) = self.input.fail_image(oldest.target) {
                self.selection.shift_composer_selection(shift);
            }
        }
        if dropped > 0 {
            self.show_toast(Toast::warning(
                "Image dropped: too many pending images",
                Duration::from_secs(4),
            ));
        }
    }
}
