//! MIME 串的归一：粘贴通道（OSC 5522 的 listing / read 回显，本地读取的类型清单）
//! 在比较名字之前都要过这一份实现 —— 各写一份就会漂移。

/// 剥掉 `;charset=…` 之类的参数、去首尾空白、转小写。
///
/// **请求侧与响应侧必须共用它**（协议机挑表示 / 比对 read DATA 的 mime 归属，本地
/// 剪贴板挑类型）：终端与剪贴板工具回显的名字不必与列出的逐字一致（参数、大小写
/// 都是允许的差异），口径一旦分叉，数据会被静默丢弃或漏选。
pub(crate) fn normalize_mime(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_parameters_whitespace_and_case() {
        assert_eq!(normalize_mime(" IMAGE/PNG;charset=utf-8 "), "image/png");
        assert_eq!(normalize_mime("image/tiff"), "image/tiff");
        assert_eq!(normalize_mime("text/plain; q=0.3"), "text/plain");
    }
}
