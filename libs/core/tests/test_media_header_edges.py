"""头部尺寸解析的结构分支：fill bytes / standalone marker / 截断 / 越界。

尺寸解析面对的是不可信文件——每个结构异常都必须返回 None（"量不出来"），
绝不抛异常（test_media.py 的 malformed 参数化只覆盖了整体截断）。
"""

from __future__ import annotations

import struct


from wing.media import image_dimensions


def _seg(marker: int, payload: bytes) -> bytes:
    return bytes([0xFF, marker]) + struct.pack(">H", len(payload) + 2) + payload


def _sof(
    width: int, height: int, *, marker: int = 0xC0, length: int | None = None
) -> bytes:
    payload = (
        b"\x08"
        + struct.pack(">HH", height, width)
        + b"\x03\x01\x11\x00\x02\x11\x01\x03\x11\x01"
    )
    if length is None:
        return _seg(marker, payload)
    return (
        bytes([0xFF, marker])
        + struct.pack(">H", length)
        + payload[: max(0, length - 2)]
    )


def _vp8_chunk(
    width: int, height: int, *, start_code: bytes = b"\x9d\x01\x2a"
) -> bytes:
    header = b"RIFF" + struct.pack("<I", 20) + b"WEBP" + b"VP8 "
    payload = (
        b"\x00\x00\x00"
        + start_code
        + struct.pack("<H", width)
        + struct.pack("<H", height)
    )
    return header + payload


class TestJpegStructure:
    def test_non_marker_byte_aborts(self):
        """段间出现非 0xFF 字节 → 不是合法 JPEG，放弃解析。"""
        assert (
            image_dimensions(b"\xff\xd8" + b"\x00" + b"\xff\xc0" * 8, "image/jpeg")
            is None
        )

    def test_fill_bytes_before_marker(self):
        """marker 前的多个 0xFF 填充字节要跳过（合法编码形态）。"""
        data = b"\xff\xd8" + b"\xff\xff\xff" + _sof(64, 32)
        assert image_dimensions(data, "image/jpeg") == (64, 32)

    def test_standalone_markers_skipped(self):
        """standalone marker（TEM / RSTn / SOI / EOI）没有长度字段，跳过 2 字节。"""
        data = b"\xff\xd8" + b"\xff\xd8" + b"\xff\x01" + _sof(11, 22)
        assert image_dimensions(data, "image/jpeg") == (11, 22)

    def test_truncated_segment_header(self):
        assert image_dimensions(b"\xff\xd8\xff\xc0\x00", "image/jpeg") is None

    def test_segment_length_beyond_end(self):
        data = (
            b"\xff\xd8" + bytes([0xFF, 0xC0]) + struct.pack(">H", 500) + b"\x08\x00\x01"
        )
        assert image_dimensions(data, "image/jpeg") is None

    def test_sof_segment_too_short(self):
        """SOF 段短于 7 字节：装不下 precision + 高宽，判失败。"""
        data = b"\xff\xd8" + _sof(0, 0, length=6)
        assert image_dimensions(data, "image/jpeg") is None

    def test_zero_dimensions_rejected(self):
        assert image_dimensions(b"\xff\xd8" + _sof(0, 16), "image/jpeg") is None

    def test_no_sof_before_end(self):
        data = b"\xff\xd8" + _seg(0xE0, b"JFIF\x00")  # APP0 之后直接结束
        assert image_dimensions(data, "image/jpeg") is None


class TestWebpStructure:
    def test_container_too_short(self):
        assert image_dimensions(b"RIFF\x00\x00\x00\x00WEB", "image/webp") is None

    def test_vp8x_payload_too_short(self):
        data = b"RIFF" + struct.pack("<I", 20) + b"WEBP" + b"VP8X" + b"\x00" * 8
        assert image_dimensions(data, "image/webp") is None

    def test_vp8_bad_start_code(self):
        assert (
            image_dimensions(
                _vp8_chunk(10, 10, start_code=b"\x00\x00\x00"), "image/webp"
            )
            is None
        )

    def test_vp8_zero_dimensions(self):
        assert image_dimensions(_vp8_chunk(0, 0), "image/webp") is None

    def test_unknown_chunk_fourcc(self):
        data = b"RIFF" + struct.pack("<I", 20) + b"WEBP" + b"ANIM" + b"\x00" * 16
        assert image_dimensions(data, "image/webp") is None


class TestParserFailureIsSwallowed:
    def test_parser_exception_becomes_none(self, monkeypatch):
        """任何解析器抛出的意外异常都按"解析失败"处理，不外溢。"""
        import wing.media.facts as facts

        def _boom(data: bytes):
            raise RuntimeError("parser bug")

        monkeypatch.setattr(facts, "_png_dimensions", _boom)

        assert (
            image_dimensions(b"\x89PNG\r\n\x1a\n" + b"\x00" * 40, "image/png") is None
        )
