//! 媒体对象的本地规则：id 形态，以及「上传回执」的预测。
//!
//! [`MediaRef`] 本身是线格式（本体在 `wing_api_client::models`）；这里只有三条纯函数，
//! 都是网关契约在客户端的镜像：
//!
//! * [`is_media_id`] —— id 的合法形态（64 位小写 hex）。与网关
//!   `wing.store.base.validate_media_id` 同一口径，是「这条引用能不能去取字节」的第一道闸门。
//! * [`predicted_ref`] —— 一份**本机已持有字节**的图片在上传之后会得到的引用。上传端点按
//!   `id = sha256(bytes)` 落盘（内容寻址），所以本机算出的 id 与响应里的 id **逐字节相同**：
//!   pending 气泡因此能在上传往返之前就画出图（字节就在手里，见 `ui/chat_view/media.rs`）。
//! * [`tolerant_media_array`] —— 媒体引用数组的宽容解码（事件与历史共用一份策略）。

use std::fmt::Write as _;

use serde::Deserialize as _;
use sha2::Digest as _;
use sha2::Sha256;

use super::MediaRef;

/// `id` 是否是合法的媒体对象 id：64 位小写 hex（= 内容的 sha256）。
///
/// 非法 id 与「对象不存在」在网关同价（404，见 `GET /api/session/media`）——客户端
/// 因此**不拿它去发请求**：气泡里那一张图退回标签行，不做一次注定失败的往返。
pub fn is_media_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 宽容解码一个媒体引用数组——`tool_media`（事件）与 `media`（历史记录）共用这一份策略。
///
/// 两层容错：`null` / 缺席 / 非数组 → 空表；数组里单条畸形（缺字段 / 类型不符）→ **只跳过
/// 该条**，保留其余合法条目与整条记录——图片是渲染装饰，一条坏数据不该让整帧事件或整条
/// 历史消息消失。
///
/// `label` 只进日志（`"<label> is not an array"` / `"skipping malformed <label> entry"`）；
/// `warn_non_array` 决定一个真·非数组值要不要也记一条：历史的 `media` 会把这种值说出来，
/// 事件的 `tool_media` 把 `null` 与缺键同价、静默忽略。
pub(crate) fn tolerant_media_array<'de, D>(
    deserializer: D,
    label: &'static str,
    warn_non_array: bool,
) -> Result<Vec<MediaRef>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let serde_json::Value::Array(entries) = value else {
        if warn_non_array && !value.is_null() {
            tracing::warn!(value = %value, "{label} is not an array, ignoring");
        }
        return Ok(Vec::new());
    };
    Ok(entries
        .into_iter()
        .filter_map(|entry| match serde_json::from_value::<MediaRef>(entry) {
            Ok(media) => Some(media),
            Err(err) => {
                tracing::warn!(error = %err, "skipping malformed {label} entry");
                None
            }
        })
        .collect())
}

/// 字节的媒体对象 id：内容的 sha256，小写 hex（与网关的 `media_id_for` 逐字节一致）。
pub fn media_id_of(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        // `write!` into a `String` cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 一份**已在本机**的图片字节在上传后会得到的引用（`id` 是内容寻址的预测）。
///
/// `mime` 由调用方给（TUI 的粘贴产物恒为 `image/jpeg`，见 `util::image_compress`）；
/// `bytes` / 尺寸取本机事实。`name` 不预测：本客户端的上传不带展示名。
///
/// 预测错了会怎样（网关改掉 id 规则）：气泡里的图取不到字节（退回 caption），消息本身
/// 照常发出——**展示降级，协议不受影响**。
pub fn predicted_ref(bytes: &[u8], mime: &str, width: u32, height: u32) -> MediaRef {
    MediaRef {
        id: media_id_of(bytes),
        mime: mime.to_string(),
        bytes: bytes.len() as u64,
        width,
        height,
        name: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_is_the_content_hash_the_gateway_computes() {
        // sha256("") = e3b0c442…（内容寻址的第一性事实：空字节也有 id）。
        assert_eq!(
            media_id_of(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(media_id_of(b"abc").len(), 64);
        assert_ne!(media_id_of(b"abc"), media_id_of(b"abd"));
        // 大小写：hex 输出恒为小写（id 闸门只收小写）。
        assert!(is_media_id(&media_id_of(b"anything")));
        assert!(
            media_id_of(b"anything")
                .bytes()
                .all(|b| !b.is_ascii_uppercase())
        );
    }

    #[test]
    fn the_id_gate_matches_the_gateways_shape() {
        let good = "a".repeat(64);
        assert!(is_media_id(&good));
        assert!(is_media_id(&"0123456789abcdef".repeat(4)));
        // 长度 / 字符集 / 大小写：任一条不合就不是 id。
        assert!(!is_media_id(""));
        assert!(!is_media_id(&"a".repeat(63)));
        assert!(!is_media_id(&"a".repeat(65)));
        assert!(!is_media_id(&"A".repeat(64)), "大写 hex 不是合法 id");
        assert!(!is_media_id(&"g".repeat(64)));
        assert!(!is_media_id(&format!("{}z", "a".repeat(63))));
        assert!(!is_media_id("../etc/passwd"));
        assert!(!is_media_id(&format!("{}/../x", "a".repeat(60))));
    }

    #[test]
    fn a_predicted_ref_carries_the_local_facts() {
        let bytes = vec![0xFFu8; 4096];
        let media = predicted_ref(&bytes, "image/jpeg", 640, 480);
        assert_eq!(media.id, media_id_of(&bytes));
        assert!(is_media_id(&media.id), "a predicted id is a real id shape");
        assert_eq!(media.mime, "image/jpeg");
        assert_eq!(media.bytes, 4096);
        assert_eq!((media.width, media.height), (640, 480));
        assert_eq!(media.name, None, "本客户端的上传不带展示名");
        // 同字节 → 同 id（内容寻址的全部意义）。
        assert_eq!(
            media.id,
            predicted_ref(&bytes, "image/jpeg", 1, 1).id,
            "id 与展示元数据无关"
        );
    }
}
