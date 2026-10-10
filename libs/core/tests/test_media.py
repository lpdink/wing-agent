"""wing.media 纯函数层单测：magic/sniff、头部尺寸解析、信封、投影算法。"""

from __future__ import annotations

import hashlib
import struct

import pytest

from wing.media import (
    BUDGET_PLACEHOLDER,
    NO_VISION_PLACEHOLDER,
    TOO_LARGE_PLACEHOLDER,
    DropReason,
    MediaAccess,
    MediaPolicy,
    estimate_image_tokens,
    encoded_len,
    format_image_envelope,
    image_dimensions,
    is_apple_cgbi_png,
    media_id,
    plan_request_media,
    sniff_image_mime,
)
from wing.schema import MediaRef, Message


########## 构造测试用图片头部（纯字节，不解码）


def png_bytes(width: int, height: int) -> bytes:
    # sig(8) + chunk len(4) + "IHDR"(4) + w(4) + h(4) + 其余 IHDR 字段
    return (
        b"\x89PNG\r\n\x1a\n"
        + struct.pack(">I", 13)
        + b"IHDR"
        + struct.pack(">II", width, height)
        + b"\x08\x06\x00\x00\x00"
    )


def gif_bytes(width: int, height: int, version: bytes = b"GIF89a") -> bytes:
    return version + struct.pack("<HH", width, height) + b"\x00\x00\x00"


def _jpeg_segment(marker: int, payload: bytes) -> bytes:
    return bytes([0xFF, marker]) + struct.pack(">H", len(payload) + 2) + payload


def jpeg_bytes(
    width: int,
    height: int,
    sof_marker: int = 0xC0,
    *,
    with_app_segments: bool = True,
) -> bytes:
    data = b"\xff\xd8"
    if with_app_segments:
        # APP0(JFIF) + APP1(Exif) + COM：解析必须逐个跳过它们找到 SOFn。
        data += _jpeg_segment(0xE0, b"JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00")
        data += _jpeg_segment(0xE1, b"Exif\x00\x00" + b"\x00" * 32)
        data += _jpeg_segment(0xFE, b"a comment")
    sof_payload = (
        b"\x08"  # precision
        + struct.pack(">HH", height, width)
        + b"\x03\x01\x22\x00\x02\x11\x01\x03\x11\x01"
    )
    data += _jpeg_segment(sof_marker, sof_payload)
    # SOS（图像数据开始）——解析到 SOFn 即返回，不受影响
    data += b"\xff\xda" + struct.pack(">H", 12)
    return data


def _riff_chunk(fourcc: bytes, payload: bytes) -> bytes:
    riff_size = 4 + 8 + len(payload)
    return (
        b"RIFF"
        + struct.pack("<I", riff_size)
        + b"WEBP"
        + fourcc
        + struct.pack("<I", len(payload))
        + payload
    )


def webp_vp8_bytes(width: int, height: int) -> bytes:
    payload = b"\x00\x00\x00" + b"\x9d\x01\x2a" + struct.pack("<HH", width, height)
    return _riff_chunk(b"VP8 ", payload)


def webp_vp8l_bytes(width: int, height: int) -> bytes:
    w1, h1 = width - 1, height - 1
    packed = bytes(
        [
            w1 & 0xFF,
            ((w1 >> 8) & 0x3F) | ((h1 & 0x03) << 6),
            (h1 >> 2) & 0xFF,
            (h1 >> 10) & 0x0F,
        ]
    )
    return _riff_chunk(b"VP8L", b"\x2f" + packed)


def webp_vp8x_bytes(width: int, height: int) -> bytes:
    payload = (
        b"\x00\x00\x00\x00"
        + struct.pack("<I", width - 1)[:3]
        + struct.pack("<I", height - 1)[:3]
    )
    return _riff_chunk(b"VP8X", payload)


def _ref(
    mid: str = "a" * 64,
    *,
    nbytes: int = 100,
    width: int = 10,
    height: int = 10,
    mime: str = "image/png",
) -> MediaRef:
    return MediaRef(id=mid, mime=mime, bytes=nbytes, width=width, height=height)


def _msg(*refs: MediaRef) -> Message:
    return Message(role="tool", tool_call_id="c", content="x", media=list(refs))


def _refs(n: int, *, nbytes: int = 12) -> list[MediaRef]:
    return [_ref(mid=f"{i:064x}", nbytes=nbytes) for i in range(n)]


########## media_id / sniff


class TestIdAndSniff:
    def test_media_id_is_sha256_hex(self):
        assert media_id(b"abc") == hashlib.sha256(b"abc").hexdigest()
        assert len(media_id(b"")) == 64

    @pytest.mark.parametrize(
        ("data", "expected"),
        [
            (png_bytes(1, 1), "image/png"),
            (jpeg_bytes(1, 1), "image/jpeg"),
            (gif_bytes(1, 1), "image/gif"),
            (gif_bytes(1, 1, version=b"GIF87a"), "image/gif"),
            (webp_vp8_bytes(1, 1), "image/webp"),
            (webp_vp8l_bytes(1, 1), "image/webp"),
        ],
    )
    def test_sniff_known_formats(self, data: bytes, expected: str):
        assert sniff_image_mime(data) == expected

    @pytest.mark.parametrize(
        "data",
        [
            b"",
            b"hello world",
            b"\xff\xd8",  # JPEG 截断（不足以构成 magic 前缀）
            b"RIFF\x00\x00\x00\x00WAVEfmt ",  # RIFF 容器但不是 WEBP
            b"GIF88a" + b"\x00" * 4,  # 版本号不对
        ],
    )
    def test_sniff_unknown_returns_none(self, data: bytes):
        assert sniff_image_mime(data) is None


########## image_dimensions


class TestDimensions:
    def test_png(self):
        assert image_dimensions(png_bytes(800, 600), "image/png") == (800, 600)

    @pytest.mark.parametrize("sof_marker", [0xC0, 0xC1, 0xC2])
    def test_jpeg_sof_variants(self, sof_marker: int):
        data = jpeg_bytes(321, 123, sof_marker=sof_marker)
        assert image_dimensions(data, "image/jpeg") == (321, 123)

    def test_jpeg_skips_app_and_comment_segments(self):
        """多个 APPn/COM 段在前——不能只看固定偏移，必须逐段扫描。"""
        with_app_segments = jpeg_bytes(640, 480, with_app_segments=True)
        assert image_dimensions(with_app_segments, "image/jpeg") == (640, 480)
        assert image_dimensions(
            jpeg_bytes(640, 480, with_app_segments=False), "image/jpeg"
        ) == (640, 480)

    def test_jpeg_sos_before_sof_returns_none(self):
        data = b"\xff\xd8" + b"\xff\xda" + struct.pack(">H", 12) + b"\x00" * 10
        assert image_dimensions(data, "image/jpeg") is None

    def test_jpeg_segment_out_of_bounds_returns_none(self):
        # APP0 声明长度远超实际数据——不能越界读，也不能抛。
        data = b"\xff\xd8" + b"\xff\xe0" + struct.pack(">H", 0x4000) + b"\x00" * 8
        assert image_dimensions(data, "image/jpeg") is None

    def test_gif(self):
        assert image_dimensions(gif_bytes(1024, 768), "image/gif") == (1024, 768)

    def test_webp_vp8(self):
        assert image_dimensions(webp_vp8_bytes(1920, 1080), "image/webp") == (
            1920,
            1080,
        )

    def test_webp_vp8l(self):
        # VP8L 是位打包（14 位宽 + 14 位高），取个非对称尺寸防写反。
        assert image_dimensions(webp_vp8l_bytes(512, 300), "image/webp") == (512, 300)

    def test_webp_vp8x(self):
        assert image_dimensions(webp_vp8x_bytes(2000, 1000), "image/webp") == (
            2000,
            1000,
        )

    @pytest.mark.parametrize(
        ("data", "mime"),
        [
            (b"", "image/png"),
            (b"\x89PNG\r\n\x1a\n", "image/png"),
            (png_bytes(800, 600)[:20], "image/png"),
            (b"", "image/jpeg"),
            (b"\xff\xd8", "image/jpeg"),
            (b"", "image/gif"),
            (gif_bytes(3, 3)[:8], "image/gif"),
            (b"", "image/webp"),
            (b"RIFF\x00\x00\x00\x00WEBP", "image/webp"),
            (webp_vp8l_bytes(4, 4)[:22], "image/webp"),
        ],
    )
    def test_malformed_returns_none_never_raises(self, data: bytes, mime: str):
        assert image_dimensions(data, mime) is None

    def test_unknown_mime_returns_none(self):
        assert image_dimensions(png_bytes(1, 1), "image/bmp") is None

    def test_zero_size_rejected(self):
        assert image_dimensions(png_bytes(0, 10), "image/png") is None
        assert image_dimensions(gif_bytes(10, 0), "image/gif") is None

    def test_apple_cgbi_png_is_recognized_but_not_parsed(self):
        """CgBI 变体（IHDR 在偏移 28）：标准解析失败，但识别为 Apple 变体（S1）。"""
        cgbi = (
            b"\x89PNG\r\n\x1a\n"
            + struct.pack(">I", 4)
            + b"CgBI"
            + b"PROF"  # CgBI chunk payload（内容不参与判定）
            + b"\x00\x00\x00\x00"  # chunk CRC（内容不参与判定）
            + b"\x00" * 16
        )
        assert image_dimensions(cgbi, "image/png") is None
        assert is_apple_cgbi_png(cgbi) is True

    def test_cgbi_predicate_needs_png_signature(self):
        """谓词自洽：签名 + 偏移 12 的 "CgBI" 两条都满足才算 CgBI。"""
        assert is_apple_cgbi_png(png_bytes(4, 4)) is False
        assert is_apple_cgbi_png(b"") is False
        assert is_apple_cgbi_png(b"\x89PNG\r\n\x1a\n") is False
        assert is_apple_cgbi_png(b"\x00" * 12 + b"CgBI" + b"\x00" * 8) is False


########## 信封 / 尺寸估算


class TestEnvelopeAndSizes:
    @pytest.mark.parametrize(
        ("nbytes", "expected"),
        [(0, 0), (1, 4), (3, 4), (4, 8), (6, 8), (7, 12), (3000, 4000)],
    )
    def test_encoded_len(self, nbytes: int, expected: int):
        assert encoded_len(nbytes) == expected

    @pytest.mark.parametrize(
        ("width", "height", "expected"),
        [(1, 1, 1), (750, 1, 1), (751, 1, 2), (100, 100, 14), (1920, 1080, 2765)],
    )
    def test_estimate_image_tokens(self, width: int, height: int, expected: int):
        assert estimate_image_tokens(_ref(width=width, height=height)) == expected

    def test_envelope_format(self):
        ref = _ref(mid="ab" * 32, nbytes=3000, width=800, height=600)
        assert (
            format_image_envelope("/tmp/a.png", ref, 1759271234)
            == "[image: /tmp/a.png | png 800x600 | 2.9 KB | id abababab | mtime 1759271234]"
        )

    @pytest.mark.parametrize(
        ("nbytes", "expected"),
        [
            (512, "512 bytes"),
            (1023, "1023 bytes"),
            (1024, "1.0 KB"),
            (1023999, "1000.0 KB"),
            (1048524, "1023.9 KB"),  # 仍 < 1024 的最大 KB 表示
            (1048525, "1.0 MB"),  # 四舍五入会进位到 1024.0 KB → 改用 MB
            (1048575, "1.0 MB"),  # 进位边界
            (1048576, "1.0 MB"),
            (2 * 1024 * 1024, "2.0 MB"),
        ],
    )
    def test_envelope_human_readable_size(self, nbytes: int, expected: str):
        ref = _ref(nbytes=nbytes)
        assert f"| {expected} |" in format_image_envelope("/x", ref, 1)

    def test_envelope_mime_short(self):
        ref = _ref(mime="image/jpeg")
        assert "| jpeg 10x10 |" in format_image_envelope("/x.jpg", ref, 1)


########## plan_request_media


class TestPlanRequestMedia:
    def test_no_media_returns_empty(self):
        assert plan_request_media([], policy=MediaPolicy(), vision=True) == []
        assert (
            plan_request_media(
                [Message(role="user", content="hi")], policy=MediaPolicy(), vision=True
            )
            == []
        )

    def test_vision_false_drops_all(self):
        msgs = [_msg(*_refs(3))]
        plans = plan_request_media(msgs, policy=MediaPolicy(), vision=False)
        assert len(plans) == 3
        assert all(p.drop_reason == DropReason.NO_VISION for p in plans)
        assert all(not p.kept for p in plans)

    def test_vision_false_wins_over_cap(self):
        """规则 0 在前：vision=False 时 even 小图也不进 cap 分支。"""
        msgs = [_msg(_ref(nbytes=1))]
        plans = plan_request_media(
            msgs, policy=MediaPolicy(), vision=False, max_image_bytes=0
        )
        assert plans[0].drop_reason == DropReason.NO_VISION

    def test_under_all_limits_keeps_everything(self):
        msgs = [_msg(*_refs(5))]
        plans = plan_request_media(msgs, policy=MediaPolicy(), vision=True)
        assert all(p.kept for p in plans)
        assert [p.message_index for p in plans] == [0] * 5

    def test_message_index_tracks_occurrence_order(self):
        msgs = [
            Message(role="user", content="q"),
            _msg(_ref(), _ref()),
            Message(role="assistant", content="a"),
            _msg(_ref()),
        ]
        plans = plan_request_media(msgs, policy=MediaPolicy(), vision=True)
        assert [p.message_index for p in plans] == [1, 1, 3]

    def test_too_large_strictly_greater(self):
        msgs = [_msg(_ref(nbytes=50), _ref(nbytes=100), _ref(nbytes=101))]
        plans = plan_request_media(
            msgs, policy=MediaPolicy(), vision=True, max_image_bytes=100
        )
        assert [p.drop_reason for p in plans] == [
            None,
            None,
            DropReason.TOO_LARGE,
        ]

    def test_too_large_excluded_from_count_budget(self):
        """被尺寸兜底丢掉的不占计数预算：eligible=1 ≤ max_images=1。"""
        msgs = [_msg(_ref(nbytes=999), _ref(nbytes=1))]
        plans = plan_request_media(
            msgs, policy=MediaPolicy(max_images=1), vision=True, max_image_bytes=100
        )
        assert plans[0].drop_reason == DropReason.TOO_LARGE
        assert plans[1].kept

    def test_count_high_water_drops_oldest_by_quantum(self):
        # 5 张 > max_images=2：excess=3 → ceil(3/2)*2 = 4 张（最旧 4 张）。
        policy = MediaPolicy(max_images=2, count_quantum=2)
        msgs = [_msg(*_refs(5))]
        plans = plan_request_media(msgs, policy=policy, vision=True)
        assert [p.drop_reason for p in plans] == [
            DropReason.BUDGET,
            DropReason.BUDGET,
            DropReason.BUDGET,
            DropReason.BUDGET,
            None,
        ]
        # 最旧优先：保留的是出现序最后一张
        assert plans[-1].ref.id == f"{4:064x}"

    def test_byte_high_water_releases_quantum_target(self):
        # 10 × 12B → encoded 16B 各；总 160 > budget 100：excess=60 →
        # target=ceil(60/50)*50=100 → 从最旧累加 16×7=112 ≥ 100 → 丢 7 张。
        policy = MediaPolicy(
            max_images=100,
            count_quantum=8,
            request_budget_bytes=100,
            evict_quantum_bytes=50,
        )
        msgs = [_msg(*_refs(10))]
        plans = plan_request_media(msgs, policy=policy, vision=True)
        dropped = [p for p in plans if not p.kept]
        kept = [p for p in plans if p.kept]
        assert len(dropped) == 7
        assert all(p.drop_reason == DropReason.BUDGET for p in dropped)
        assert [p.ref.id for p in kept] == [f"{i:064x}" for i in (7, 8, 9)]

    def test_byte_rule_covers_target_even_when_overshooting(self):
        """释放量按累加覆盖（单张大于目标也只需丢一张）。"""
        policy = MediaPolicy(
            max_images=100,
            count_quantum=8,
            request_budget_bytes=10,
            evict_quantum_bytes=1000,
        )
        msgs = [_msg(_ref(nbytes=9), _ref(nbytes=3), _ref(nbytes=3))]
        plans = plan_request_media(msgs, policy=policy, vision=True)
        # total=12+4+4=20 > 10 → excess=10 → target=1000；最旧一张 12B 不够，
        # 继续累加直到覆盖（全丢也不能覆盖 → eligible 全丢）。
        assert all(not p.kept for p in plans)

    def test_two_rules_take_more_aggressive(self):
        # 计数规则：10 > max_images=8，count_quantum=2 → excess=2 → 丢 2。
        # 字节规则：10×100B → encoded 136B 各，总 1360 > 1000 → excess=360 →
        # target=ceil(360/100)*100=400 → 累加 136×3=408 ≥ 400 → 丢 3。
        # 取更激进者 → 丢 3 张。
        policy = MediaPolicy(
            max_images=8,
            count_quantum=2,
            request_budget_bytes=1000,
            evict_quantum_bytes=100,
        )
        msgs = [_msg(*_refs(10, nbytes=100))]
        plans = plan_request_media(msgs, policy=policy, vision=True)
        dropped = [p for p in plans if not p.kept]
        assert len(dropped) == 3
        # 丢弃恒为最旧优先
        assert [p.ref.id for p in dropped] == [f"{i:064x}" for i in (0, 1, 2)]

    def test_count_rule_more_aggressive_than_byte_rule(self):
        policy = MediaPolicy(
            max_images=2,
            count_quantum=8,
            request_budget_bytes=10**9,
            evict_quantum_bytes=10**6,
        )
        msgs = [_msg(*_refs(10))]
        plans = plan_request_media(msgs, policy=policy, vision=True)
        assert sum(1 for p in plans if not p.kept) == 8
        assert [p.ref.id for p in plans if p.kept] == [f"{i:064x}" for i in (8, 9)]

    def test_deterministic_same_input_same_output(self):
        policy = MediaPolicy(max_images=2, count_quantum=2)
        msgs = [_msg(*_refs(6)), _msg(*_refs(3))]
        first = plan_request_media(
            msgs, policy=policy, vision=True, max_image_bytes=1000
        )
        second = plan_request_media(
            msgs, policy=policy, vision=True, max_image_bytes=1000
        )
        assert first == second
        # 投影是纯函数：不改写消息本身
        assert len(msgs[0].media or []) == 6
        assert len(msgs[1].media or []) == 3

    def test_dropped_leaves_message_bytes_untouched(self):
        """投影只做判定——消息的 content/media 在丢弃前后完全一致。"""
        ref = _ref()
        msg = _msg(ref)
        before = msg.model_dump(mode="json")
        plan_request_media([msg], policy=MediaPolicy(), vision=False)
        assert msg.model_dump(mode="json") == before

    def test_default_policy_values_are_frozen_contract(self):
        policy = MediaPolicy()
        assert policy.max_images == 32
        assert policy.count_quantum == 8
        assert policy.request_budget_bytes == 37_748_736
        assert policy.evict_quantum_bytes == 18_874_368


########## 占位文案 / MediaAccess


class TestPlaceholdersAndAccess:
    def test_placeholder_texts(self):
        assert NO_VISION_PLACEHOLDER.startswith("(image omitted")
        assert "budget" in BUDGET_PLACEHOLDER
        assert "re-read the file" in BUDGET_PLACEHOLDER
        assert "size limit" in TOO_LARGE_PLACEHOLDER

    def test_media_access_roundtrip(self):
        pool: dict[str, bytes] = {}
        access = MediaAccess(
            read=lambda mid: pool.get(mid),
            write=lambda mid, data: pool.__setitem__(mid, data),
        )
        access.write("a" * 64, b"data")
        assert access.read("a" * 64) == b"data"
        assert access.read("b" * 64) is None
