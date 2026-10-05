# wing/media/facts.py
"""字节 → 事实：sha256 id / mime 嗅探 / 尺寸解析 / 信封文本与尺寸估算。

纯函数与不可变数据：不依赖 config、不读存储、不做投影决策（投影见 ``policy``）。
"""

from __future__ import annotations

import hashlib

from wing.schema import MediaRef


########## 字节 → 事实


def media_id(data: bytes) -> str:
    """内容的 sha256 hex——即存储文件名（内容寻址，天然去重）。"""
    return hashlib.sha256(data).hexdigest()


_PNG_SIG = b"\x89PNG\r\n\x1a\n"

SUPPORTED_IMAGE_MIMES: frozenset[str] = frozenset(
    {"image/png", "image/jpeg", "image/webp", "image/gif"}
)
"""读图链路支持的四种格式（= ``sniff_image_mime`` 的值域）。

ReadImage 的报错文案与 Read 的图片指引共用此集合，支持格式列表不出现第二份。
"""


def sniff_image_mime(data: bytes) -> str | None:
    """按 magic bytes 判定图片格式；不认识/太短返回 None。

    只认四种格式（PNG/JPEG/GIF/WebP）——读图链路支持的集合即此集合，
    文件后缀名不参与判定。
    """
    if data.startswith(_PNG_SIG):
        return "image/png"
    if data.startswith(b"\xff\xd8\xff"):
        return "image/jpeg"
    if data.startswith((b"GIF87a", b"GIF89a")):
        return "image/gif"
    if len(data) >= 12 and data.startswith(b"RIFF") and data[8:12] == b"WEBP":
        return "image/webp"
    return None


def image_dimensions(data: bytes, mime: str) -> tuple[int, int] | None:
    """纯 Python 头部解析图片尺寸；不支持的 mime 或结构异常返回 None（永不抛）。

    四种格式的解析位置（都只读头部，不解码像素）：
    - PNG：固定 IHDR 位置（signature 8 + chunk length 4 + "IHDR" 4）。
    - GIF：logical screen descriptor（小端 uint16 ×2）。
    - JPEG：逐段扫描到 SOFn（跳过 APPn / COM 等，覆盖 SOF0/1/2 及全部
      SOF 变体），SOS 之前找不到即放弃。
    - WebP：RIFF 容器内 VP8（有损）/ VP8L（无损）/ VP8X（扩展画布）三种 chunk。
    """
    try:
        if mime == "image/png":
            return _png_dimensions(data)
        if mime == "image/jpeg":
            return _jpeg_dimensions(data)
        if mime == "image/gif":
            return _gif_dimensions(data)
        if mime == "image/webp":
            return _webp_dimensions(data)
    except Exception:
        # 纯头部解析面对的是不可信文件——任何意外都按"解析失败"处理，
        # 绝不能把异常外溢到调用方（工具/请求路径）。
        return None
    return None


def is_apple_cgbi_png(data: bytes) -> bool:
    """是否 Apple CgBI 变体 PNG（非标准：签名后紧跟 ``CgBI`` chunk）。

    该类文件的 IHDR 出现在偏移 28 而不是 16，标准 PNG 布局解析器一律失败；
    但文件本身完全合法可显示——读图链路据此给出"变体需转换"的准确指引，
    而不是误报"头部截断或损坏"。

    刻意**不**尝试解析 CgBI 的尺寸：其字节序是 Apple 私有约定，没有可验证的
    规范来源（见 docs/dev/media-images.md「已知边界」）。只识别、不解析。
    """
    return data.startswith(_PNG_SIG) and data[12:16] == b"CgBI"


def _png_dimensions(data: bytes) -> tuple[int, int] | None:
    # PNG: sig(8) + chunk length(4) + "IHDR"(4) + width(4) + height(4)，均大端。
    if len(data) < 24 or data[12:16] != b"IHDR":
        return None
    width = int.from_bytes(data[16:20], "big")
    height = int.from_bytes(data[20:24], "big")
    if width <= 0 or height <= 0:
        return None
    return width, height


def _gif_dimensions(data: bytes) -> tuple[int, int] | None:
    # GIF: "GIF87a"/"GIF89a"(6) + logical screen width(2) + height(2)，小端。
    if len(data) < 10:
        return None
    width = int.from_bytes(data[6:8], "little")
    height = int.from_bytes(data[8:10], "little")
    if width <= 0 or height <= 0:
        return None
    return width, height


_JPEG_SOF_MARKERS = frozenset(
    {
        0xC0,
        0xC1,
        0xC2,
        0xC3,
        0xC5,
        0xC6,
        0xC7,
        0xC9,
        0xCA,
        0xCB,
        0xCD,
        0xCE,
        0xCF,
    }
)


def _jpeg_dimensions(data: bytes) -> tuple[int, int] | None:
    """逐段扫描 JPEG 到 SOFn（跳过 APPn/COM 等），读帧头里的高/宽。"""
    i = 2  # 跳过 SOI（FFD8）
    n = len(data)
    while i + 1 < n:
        if data[i] != 0xFF:
            return None
        marker = data[i + 1]
        if marker == 0xFF:
            # 段间允许填充多个 0xFF（marker 前的 fill bytes）。
            i += 1
            continue
        if marker == 0x00 or marker == 0x01 or 0xD0 <= marker <= 0xD9:
            # standalone marker（TEM / RSTn / SOI / EOI）：无长度字段。
            i += 2
            continue
        if marker == 0xDA:
            # SOS：之后是熵编码数据，SOF 不会再出现。
            return None
        if i + 4 > n:
            return None
        seg_len = int.from_bytes(data[i + 2 : i + 4], "big")
        if seg_len < 2 or i + 2 + seg_len > n:
            return None
        if marker in _JPEG_SOF_MARKERS:
            if seg_len < 7:
                return None
            # 段内布局：length(2) + precision(1) + height(2) + width(2)。
            height = int.from_bytes(data[i + 5 : i + 7], "big")
            width = int.from_bytes(data[i + 7 : i + 9], "big")
            if width <= 0 or height <= 0:
                return None
            return width, height
        i += 2 + seg_len
    return None


def _webp_dimensions(data: bytes) -> tuple[int, int] | None:
    # RIFF 容器：0-3 "RIFF" + 4-7 size + 8-11 "WEBP" + 12-15 chunk fourcc。
    if len(data) < 16:
        return None
    fourcc = data[12:16]
    if fourcc == b"VP8X":
        # payload(20): 1B flags + 3B reserved + 3B (w-1) + 3B (h-1)，小端 24 位。
        if len(data) < 30:
            return None
        width = int.from_bytes(data[24:27], "little") + 1
        height = int.from_bytes(data[27:30], "little") + 1
        return width, height
    if fourcc == b"VP8L":
        # payload(20): 1B signature(0x2F) + 4B 位打包（各 14 位，w-1 在前）。
        if len(data) < 25 or data[20] != 0x2F:
            return None
        b0, b1, b2, b3 = data[21], data[22], data[23], data[24]
        width = (b0 | ((b1 & 0x3F) << 8)) + 1
        height = (((b1 >> 6) & 0x03) | (b2 << 2) | ((b3 & 0x0F) << 10)) + 1
        return width, height
    if fourcc == b"VP8 ":
        # payload(20): 3B frame tag + 3B 起始码 9D 01 2A + 2B 宽(14 位) + 2B 高。
        if len(data) < 30 or data[23:26] != b"\x9d\x01\x2a":
            return None
        width = int.from_bytes(data[26:28], "little") & 0x3FFF
        height = int.from_bytes(data[28:30], "little") & 0x3FFF
        if width <= 0 or height <= 0:
            return None
        return width, height
    return None


########## 信封 / 尺寸估算


_MIME_SHORT = {
    "image/png": "png",
    "image/jpeg": "jpeg",
    "image/gif": "gif",
    "image/webp": "webp",
}


def format_size(nbytes: int) -> str:
    """人类可读字节数（1024 进制，一位小数）。

    < 1 KiB 保留 "N bytes" 字面形态；KB 级数值四舍五入后若进位到 1024.0
    （如 1048575 B → "1024.0 KB"）则改用 MB 表达同一数值——同一单位内的
    数值恒 < 1024（review r1 N3）。图片链路单图上限是 MiB 级配置，不设
    GB 单位。信封文本与 ReadImage 的大小报错共用此函数。
    """
    if nbytes < 1024:
        return f"{nbytes} bytes"
    if nbytes < 1024 * 1024:
        kb = round(nbytes / 1024, 1)
        if kb < 1024.0:
            return f"{kb:.1f} KB"
    return f"{nbytes / (1024 * 1024):.1f} MB"


def format_image_envelope(path: str, ref: MediaRef, mtime: int) -> str:
    """模型可见的图片信封文本（单行，即工具结果的 content）。

    形如：``[image: /tmp/a.png | png 800x600 | 2.4 MB | id ab12cd34 | mtime 1759271234]``
    ——不含 base64；字节只存在 SessionStore，模型按 id 重读。
    """
    mime_short = _MIME_SHORT.get(ref.mime, ref.mime)
    return (
        f"[image: {path} | {mime_short} {ref.width}x{ref.height} | "
        f"{format_size(ref.bytes)} | id {ref.id[:8]} | mtime {mtime}]"
    )


def encoded_len(nbytes: int) -> int:
    """base64 编码后的长度：``4*ceil(n/3)``。

    投影层不读字节、不做编码——只靠它计算请求体量（与真实编码长度全等）。
    """
    return 4 * ((nbytes + 2) // 3)


def estimate_image_tokens(ref: MediaRef) -> int:
    """估算一张图片占用的 token 数：``ceil(w*h/750)``（保守经验口径）。"""
    return (ref.width * ref.height + 749) // 750
