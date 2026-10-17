//! 一张图片的**身份**：本地文件，还是网关上的会话媒体对象。
//!
//! 两者对绘制层只有一点不同——字节从哪来——布局、盒高、绘制原语完全共用：
//!
//! ```text
//! File(path)   本地文件：字节随文件走，可被改写（mtime 是缓存身份的一部分），
//!              形状来自头信息探测（`ImageOpts.shapes`）。
//! Media(id)    网关会话媒体对象：内容寻址（`id == sha256(bytes)`，见
//!              `crate::protocol::media`），**字节不可变** —— 同一个 id 永远是同一份
//!              字节，不需要新鲜度检查，形状由 `MediaRef` 自带（不用探测）。
//! ```
//!
//! 锚点（`render::markdown::ImageSpan`）、帧内绘制请求（`ui::chat_view::FrameImage`）
//! 都只携带这个身份，不携带字节：字节的取用与解码在 `ui::image` / `app::images` 里。
//! 放在 `render` 而不是 `ui` 是因为**布局层**（markdown 锚点与用户消息气泡）先要知道
//! 「盒子里放的是哪张图」，绘制层才拿它去换像素。

use std::path::{Path, PathBuf};

/// 一张图片的身份。
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PictureSource {
    /// 本地文件：markdown 图片经 [`resolve_image_path`](crate::render::markdown::resolve_image_path)
    /// 归一化后的路径（元数据表与绘制层共用同一个 key）。
    File(PathBuf),
    /// 网关会话媒体对象：`MediaRef.id`（64 位小写 hex 的内容 sha256）。
    Media(String),
}

impl PictureSource {
    /// 本地路径，当且仅当这是一个文件来源。
    pub fn as_file(&self) -> Option<&Path> {
        match self {
            Self::File(path) => Some(path),
            Self::Media(_) => None,
        }
    }

    /// 媒体 id，当且仅当这是一个媒体对象。
    pub fn as_media(&self) -> Option<&str> {
        match self {
            Self::File(_) => None,
            Self::Media(id) => Some(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_sources_never_compare_equal() {
        // 一个名为 `abc…` 的文件与一个 id 为 `abc…` 的媒体对象是两张不同的图。
        let file = PictureSource::File(PathBuf::from("/x/pic.png"));
        let media = PictureSource::Media("/x/pic.png".into());
        assert_ne!(file, media);
        assert_eq!(file.as_file(), Some(Path::new("/x/pic.png")));
        assert_eq!(media.as_media(), Some("/x/pic.png"));
        assert!(media.as_file().is_none());
        assert!(file.as_media().is_none());
    }

    #[test]
    fn the_source_keys_a_map_by_identity() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        assert!(seen.insert(PictureSource::File(PathBuf::from("/a.png"))));
        assert!(!seen.insert(PictureSource::File(PathBuf::from("/a.png"))));
        let id = "a".repeat(64);
        assert!(seen.insert(PictureSource::Media(id.clone())));
        assert!(!seen.insert(PictureSource::Media(id)));
        assert_eq!(seen.len(), 2);
    }
}
