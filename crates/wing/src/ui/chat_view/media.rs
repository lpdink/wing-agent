//! 消息里的图片：正文之后的一行一张标签行。
//!
//! 一条 user 消息的**显示文本** = 正文 + 每张图一行 `[Image #N W×H SIZE]`
//! （[`text_with_labels`]）。两条路径构造同一段文本——本客户端发出去的消息
//! （pending / promote，走 [`labels`]）与从历史重放的消息（replay，走
//! [`labels_of_media`]）——否则同一条消息在 resume 前后会长得不一样。
//!
//! 标签的形状归 [`crate::util::format::image_label`]（composer 的 chip 用的是同一份
//! 记法）；这里只决定**号码**与**排列**：号码是**消息内** 1-based 序号（按媒体在消息里
//! 的顺序），与草稿的 chip 号码是两套口径——重放只有媒体列表，没有草稿号码，而消息的
//! 标签该由消息自己决定。
//!
//! 没有图片时 [`text_with_labels`] 恒等返回正文：那是「无 media 的消息渲染与现状逐字节
//! 一致」这条红线的落点（cell 的文本就是它）。

use crate::protocol::MediaRef;
use crate::util::format;
use crate::util::image_compress::CompressedImage;

/// 消息的显示文本：正文 + 每张图一行标签（正文为空时只有标签行）。
///
/// 图片排在正文**之后**（D5：不承诺文本与图片的交错顺序，后端也是两个字段）。
pub fn text_with_labels(content: &str, labels: &[String]) -> String {
    if labels.is_empty() {
        return content.to_string();
    }
    let mut text = String::from(content);
    for label in labels {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(label);
    }
    text
}

/// 本地（活体）消息的标签：压缩产物按提交顺序编号。
pub fn labels(images: &[CompressedImage]) -> Vec<String> {
    images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            format::image_label(
                index + 1,
                image.width,
                image.height,
                image.bytes.len() as u64,
            )
        })
        .collect()
}

/// 重放消息的标签：历史投影里的媒体引用按消息内顺序编号（尺寸与字节数都取引用里的
/// 提示性元数据——真正发出去的字节在存储里，这里只需要那行文案）。
pub fn labels_of_media(media: &[MediaRef]) -> Vec<String> {
    media
        .iter()
        .enumerate()
        .map(|(index, media)| {
            format::image_label(index + 1, media.width, media.height, media.bytes)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(width: u32, height: u32, bytes: usize) -> CompressedImage {
        CompressedImage {
            bytes: vec![0xFF; bytes],
            width,
            height,
        }
    }

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

    #[test]
    fn labels_number_the_pictures_by_their_place_in_the_message() {
        let images = [picture(1024, 768, 393_216), picture(8, 8, 7)];
        assert_eq!(
            labels(&images),
            ["[Image #1 1024×768 384KB]", "[Image #2 8×8 7B]"],
            "1-based，按消息内顺序"
        );
        // 重放侧同一条消息给出同一串（历史引用只有元数据）。
        let refs = [media(1024, 768, 393_216), media(8, 8, 7)];
        assert_eq!(labels_of_media(&refs), labels(&images), "live == replay");
        assert!(labels(&[]).is_empty());
        assert!(labels_of_media(&[]).is_empty());
    }

    #[test]
    fn text_without_labels_is_the_content_itself() {
        // 红线：无 media 的消息，显示文本**就是**正文（cell 逐字节不变）。
        assert_eq!(text_with_labels("hello", &[]), "hello");
        assert_eq!(text_with_labels("", &[]), "");
        assert_eq!(text_with_labels("多行\n正文", &[]), "多行\n正文");
    }

    #[test]
    fn labels_land_after_the_text_one_per_line() {
        let labels = labels(&[picture(1024, 768, 393_216), picture(8, 8, 7)]);
        assert_eq!(
            text_with_labels("看这两张", &labels),
            "看这两张\n[Image #1 1024×768 384KB]\n[Image #2 8×8 7B]"
        );
    }

    #[test]
    fn a_picture_only_message_is_its_labels() {
        // 纯图消息（content 为空）合法：显示文本就是标签行，不带前导空行。
        let labels = labels(&[picture(8, 8, 7)]);
        assert_eq!(text_with_labels("", &labels), "[Image #1 8×8 7B]");
    }

    #[test]
    fn the_label_shape_is_the_composers_own() {
        // 与 composer chip 同串（同一份记法、不同的号码口径）。
        assert_eq!(
            labels(&[picture(1024, 768, 393_216)])[0],
            format::image_label(1, 1024, 768, 393_216)
        );
    }
}
