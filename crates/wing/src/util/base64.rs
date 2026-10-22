//! base64（RFC 4648 标准字母表 + padding；自己实现，不为此引依赖）。
//!
//! 两条线上通道共用它：OSC 7501 程序状态里的 `msg=`（只需编码）与 OSC 5522
//! 剪贴板协议（编码 + 严格解码）。同一条编码规则只能有一份实现——两份各带一套
//! 向量测试，改一处就会让两条通道对同一份内容给出不同的字节。

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 标准 base64 编码（带 padding：规范允许省略，但线上其他实现都带）。
pub(crate) fn encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        let n = u32::from_be_bytes([0, a, b, c]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn value_of(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// 严格解码：只认标准字母表与末尾 padding；非字母字符、`%4 == 1` 的残缺、
/// padding 数量对不上都判失败（坏数据走降级路径，绝不 panic）。
pub(crate) fn decode(text: &str) -> Option<Vec<u8>> {
    let mut body = text.as_bytes();
    let mut pad = 0usize;
    while let [head @ .., b'='] = body {
        body = head;
        pad += 1;
        if pad > 2 {
            return None;
        }
    }
    if body.len() % 4 == 1 {
        return None;
    }
    if pad > 0 && !(body.len() + pad).is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() / 4 * 3 + 3);
    for chunk in body.chunks(4) {
        let mut acc = 0u32;
        for &c in chunk {
            acc = (acc << 6) | value_of(c)? as u32;
        }
        // 每组的有效位是 6×字符数；输出第 i 个字节取从高位数起的那一段。
        let bits = 6 * chunk.len() as u32;
        for i in 0..chunk.len() - 1 {
            out.push((acc >> (bits - 8 * (i as u32 + 1))) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 §10 的标准向量（编码 + 解码往返）。
    #[test]
    fn rfc4648_vectors_round_trip() {
        for (raw, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(raw.as_bytes()), encoded, "{raw:?}");
            assert_eq!(decode(encoded).as_deref(), Some(raw.as_bytes()), "{raw:?}");
        }
    }

    #[test]
    fn multi_byte_utf8_is_not_split() {
        assert_eq!(encode("你好".as_bytes()), "5L2g5aW9");
    }

    /// 省略 padding 的形态（协议允许）能解码；残缺与非法字符一律失败。
    #[test]
    fn decode_is_strict_about_shape() {
        assert_eq!(decode("Zm9vYg").unwrap(), b"foob");
        for bad in ["Zg=", "Z", "Zg===", "Zg==x", "Z=9v", "Zm9v!"] {
            assert_eq!(decode(bad), None, "{bad:?} 必须判失败");
        }
    }
}
