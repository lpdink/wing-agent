//! App 侧的粘贴通道边界：`TermEvent::Clipboard` 的唯一落点。
//!
//! 文本与 bracketed paste 走**同一个**入口（[`App::handle_paste`]），语义逐点等价；
//! 图片先「不丢」地暂存（[`PendingClipboardImage`]），由后续步骤把它变成 composer 的
//! 图片 chip；失败一律降级为 toast，绝不动用户已有内容。
//!
//! 这一层刻意薄：协议 / 通道 / 超时全在 [`crate::tui::clipboard_protocol`]，这里只有
//! 「读回的东西怎么进 UI」这一件事，改动面与发送链路（另一波消费 composer 的产品）
//! 零重叠。

use std::time::Duration;

/// 暂存区的上限：**条数**与**总字节**。
///
/// 暂存是「不丢」的落点、不是仓库：05 接手把它变成 chip 并在提交后清空，但在这之前
/// 用户可能反复粘贴，而单张图最大 64 MiB（`clipboard_protocol::MAX_PAYLOAD`）。到顶时
/// 丢掉**最旧**的一张（FIFO）并记日志：新的一次粘贴是用户的当下意图，优先保住。
pub(crate) const MAX_PENDING_IMAGES: usize = 8;
pub(crate) const MAX_PENDING_IMAGE_BYTES: usize = 64 * 1024 * 1024;

use crate::tui::ClipboardFailure;
use crate::tui::ClipboardPayload;
use crate::ui::toast::Toast;

use super::App;

/// 终端协议通道读回、还没有转成 chip 的图片（05 消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingClipboardImage {
    /// 原始字节（未压缩；压缩在 05 的 chip 路径上做）。
    pub bytes: Vec<u8>,
    /// 终端给出的 MIME（如 `image/png`）。
    pub mime: String,
    /// 文件名——OSC 5522 不带这个信息，恒为 `None`（05 若要可自行派生）。
    pub name: Option<String>,
}

impl App {
    /// `TermEvent::Clipboard` 的分派：文本 → composer（或当前模态接受者），图片 → 暂存，
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

    /// 图片载荷边界：本步骤只把字节原样收下（05 接手转成 chip）。
    ///
    /// 暂存而不是立即丢弃是本步骤的全部承诺——「不丢」优先于「能看」。
    pub(crate) fn on_clipboard_image(
        &mut self,
        bytes: Vec<u8>,
        mime: String,
        name: Option<String>,
    ) {
        tracing::info!(
            bytes = bytes.len(),
            mime = %mime,
            "clipboard image stashed (awaiting the composer image chip)"
        );
        let size = human_bytes(bytes.len());
        self.pending_clipboard_images.push(PendingClipboardImage {
            bytes,
            mime: mime.clone(),
            name,
        });
        self.trim_pending_images();
        self.show_toast(Toast::info(
            format!("Image pasted ({mime} · {size})"),
            Duration::from_secs(3),
        ));
    }

    /// 通道失败（权限 / 超时 / 坏数据）：toast + 日志，别的一个都不动。
    pub(crate) fn on_clipboard_failure(&mut self, failure: ClipboardFailure) {
        tracing::warn!(failure = ?failure, "clipboard read failed");
        self.show_toast(Toast::warning(failure.message(), Duration::from_secs(4)));
    }

    /// 暂存的图片（05 的消费口；本步骤只由测试读取）。
    pub fn pending_clipboard_images(&self) -> &[PendingClipboardImage] {
        &self.pending_clipboard_images
    }

    /// 把暂存区收进上限（FIFO 丢最旧的，见 [`MAX_PENDING_IMAGES`]）。
    fn trim_pending_images(&mut self) {
        let mut total: usize = self
            .pending_clipboard_images
            .iter()
            .map(|i| i.bytes.len())
            .sum();
        while self.pending_clipboard_images.len() > MAX_PENDING_IMAGES
            || total > MAX_PENDING_IMAGE_BYTES
        {
            if self.pending_clipboard_images.len() == 1 {
                // 最后一张再大也留着：它就是用户刚粘的那一张。
                break;
            }
            let dropped = self.pending_clipboard_images.remove(0);
            total -= dropped.bytes.len();
            tracing::warn!(
                bytes = dropped.bytes.len(),
                mime = %dropped.mime,
                "clipboard image stash over budget, dropped the oldest"
            );
        }
    }
}

/// 人类可读的字节数（toast 文案用；与 CLI 的表格记法无关，故不放进 `util::format`）。
fn human_bytes(n: usize) -> String {
    const KIB: f64 = 1024.0;
    let n = n as f64;
    if n < KIB {
        format!("{n:.0} B")
    } else if n < KIB * KIB {
        format!("{:.1} KiB", n / KIB)
    } else {
        format!("{:.1} MiB", n / (KIB * KIB))
    }
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MiB");
    }
}
