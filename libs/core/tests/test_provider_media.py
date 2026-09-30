"""provider 请求期媒体序列化测试（openai / anthropic 两条线格式）。

覆盖 step 05 验证清单：inline / followup 线格式、能力投影（vision=False）、
单图上限（image_max_bytes）、高水位量子驱逐、字节缺失降级、压缩剥离、
cache_control 落点，以及无媒体快路径的字节级不变性。
"""

from __future__ import annotations

import base64
import hashlib
import json
import struct
from typing import Any

import pytest

from wing.compactor import Compactor
from wing.config import ModelCapabilities, ModelSpec, ProviderConfig
from wing.media import (
    BUDGET_PLACEHOLDER,
    NO_VISION_PLACEHOLDER,
    TOO_LARGE_PLACEHOLDER,
    MediaAccess,
    MediaPolicy,
    plan_request_media,
)
from wing.provider.anthropic import AnthropicProvider
from wing.provider.media import (
    FOLLOWUP_GUIDE_TEXT,
    UNAVAILABLE_PLACEHOLDER,
    plan_for_request,
    resolve_image_delivery,
)
from wing.provider.openai_compat import OpenAICompatProvider
from wing.schema import LLMResponse, MediaRef, Message, ToolUseBlock

VISION_MODEL = "vl-model"
TEXT_MODEL = "text-model"


########## 构造工具


def png_bytes(width: int, height: int, payload: bytes = b"") -> bytes:
    """最小 PNG 前缀（只求 bytes 事实，不真解码）。"""
    return (
        b"\x89PNG\r\n\x1a\n"
        + struct.pack(">I", 13)
        + b"IHDR"
        + struct.pack(">II", width, height)
        + b"\x08\x06\x00\x00\x00"
        + payload
    )


_PNG_HEADER_LEN = len(png_bytes(1, 1))


def sized_png(size: int) -> bytes:
    """总长度恰为 ``size`` 字节的 PNG 前缀（字节预算测试需要精确 encoded_len）。"""
    return png_bytes(4, 4, b"x" * (size - _PNG_HEADER_LEN))


def make_ref(
    data: bytes, width: int = 4, height: int = 4, name: str = "a.png"
) -> MediaRef:
    return MediaRef(
        id=hashlib.sha256(data).hexdigest(),
        mime="image/png",
        bytes=len(data),
        width=width,
        height=height,
        name=name,
    )


def make_media(*datas: bytes) -> MediaAccess:
    """以 sha256 为键的内存媒体池（只实现 read 实际语义）。"""
    store = {hashlib.sha256(d).hexdigest(): d for d in datas}
    return MediaAccess(read=store.get, write=store.__setitem__)


def openai_cfg(**overrides: Any) -> ProviderConfig:
    base: dict[str, Any] = {
        "name": "t-openai",
        "protocol": "openai",
        "base_url": "https://api.example.com",
        "api_key": "sk-test",
        "explicit_cache_mode": False,
        "models": [
            ModelSpec(name=VISION_MODEL, capabilities=ModelCapabilities(vision=True)),
            TEXT_MODEL,
        ],
    }
    base.update(overrides)
    return ProviderConfig(**base)


def anthropic_cfg(**overrides: Any) -> ProviderConfig:
    base: dict[str, Any] = {
        "name": "t-anthropic",
        "protocol": "anthropic",
        "base_url": "https://api.anthropic.com",
        "api_key": "sk-test",
        "explicit_cache_mode": False,
        "models": [
            ModelSpec(name=VISION_MODEL, capabilities=ModelCapabilities(vision=True)),
            TEXT_MODEL,
        ],
    }
    base.update(overrides)
    return ProviderConfig(**base)


async def openai_body(
    messages: list[Message],
    *,
    cfg: ProviderConfig | None = None,
    model: str = VISION_MODEL,
    media: MediaAccess | None = None,
) -> dict:
    p = OpenAICompatProvider(cfg or openai_cfg(), media=media)
    try:
        return p._build_body(messages, model, None, False)
    finally:
        await p.aclose()


async def anthropic_messages(
    messages: list[Message],
    *,
    cfg: ProviderConfig | None = None,
    model: str = VISION_MODEL,
    media: MediaAccess | None = None,
) -> tuple[str, list[dict]]:
    p = AnthropicProvider(cfg or anthropic_cfg(), media=media)
    try:
        return p._serialize_messages(messages, model)
    finally:
        await p.aclose()


def content_parts(msg: dict) -> list[dict]:
    """取出消息 content（字符串视为单 text part，便于统一断言）。"""
    content = msg["content"]
    if isinstance(content, str):
        return [{"type": "text", "text": content}]
    return content


def image_urls(msg: dict) -> list[str]:
    return [
        p["image_url"]["url"] for p in content_parts(msg) if p["type"] == "image_url"
    ]


def b64_of(url: str) -> bytes:
    prefix, data = url.split(",", 1)
    assert prefix.startswith("data:image/png;base64")
    return base64.b64decode(data)


def tool_messages(n: int) -> list[Message]:
    """n 条连续 tool 消息（同一轮的 n 个 tool call 结果）。"""
    return [
        Message(role="tool", tool_call_id=f"c{i}", content=f"result-{i}")
        for i in range(n)
    ]


########## resolve_image_delivery


class TestResolveImageDelivery:
    def test_explicit_config_wins(self):
        assert resolve_image_delivery(openai_cfg(image_delivery="inline")) == "inline"
        assert (
            resolve_image_delivery(anthropic_cfg(image_delivery="followup"))
            == "followup"
        )

    def test_protocol_defaults(self):
        assert resolve_image_delivery(openai_cfg()) == "followup"
        assert resolve_image_delivery(anthropic_cfg()) == "inline"


########## OpenAI 兼容线格式


class TestOpenAIMediaSerialization:
    @pytest.mark.asyncio
    async def test_inline_emits_text_then_image_url(self):
        """inline：tool 消息 content = [原文本, image_url]；data URL 解码 == 原始字节。"""
        data = png_bytes(4, 4, b"inline")
        ref = make_ref(data)
        msgs = [
            Message(role="user", content="q"),
            Message(
                role="assistant",
                content_blocks=[ToolUseBlock(id="c1", name="ReadImage", input={})],
            ),
            Message(
                role="tool",
                tool_call_id="c1",
                content="[image: /tmp/a.png | png 4x4]",
                media=[ref],
            ),
        ]
        body = await openai_body(
            msgs,
            cfg=openai_cfg(image_delivery="inline"),
            media=make_media(data),
        )
        tool_msg = body["messages"][2]
        parts = content_parts(tool_msg)
        assert [p["type"] for p in parts] == ["text", "image_url"]
        assert parts[0]["text"] == "[image: /tmp/a.png | png 4x4]"  # 原文本一字节不改
        assert b64_of(image_urls(tool_msg)[0]) == data

    @pytest.mark.asyncio
    async def test_followup_default_places_images_after_all_tool_messages(self):
        """followup（openai 默认）：tool 消息只留文本；图片汇总为段后一条 user 消息。

        同一轮多 tool call 多图 → 只有一条 user 消息，顺序 == tool 顺序；
        图片排在全部 tool 消息之后（不会插在两条 tool 之间）。
        """
        d1, d2, d3 = png_bytes(4, 4, b"1"), png_bytes(4, 4, b"2"), png_bytes(4, 4, b"3")
        r1, r2, r3 = make_ref(d1), make_ref(d2), make_ref(d3)
        msgs = [
            Message(role="user", content="q"),
            Message(
                role="assistant",
                content_blocks=[
                    ToolUseBlock(id="c1", name="ReadImage", input={}),
                    ToolUseBlock(id="c2", name="ReadImage", input={}),
                ],
            ),
            Message(role="tool", tool_call_id="c1", content="res-1", media=[r1]),
            Message(role="tool", tool_call_id="c2", content="res-2", media=[r2]),
            Message(
                role="assistant",
                content_blocks=[ToolUseBlock(id="c3", name="ReadImage", input={})],
            ),
            Message(role="tool", tool_call_id="c3", content="res-3", media=[r3]),
        ]
        body = await openai_body(msgs, media=make_media(d1, d2, d3))
        wire = body["messages"]

        assert [m["role"] for m in wire] == [
            "user",
            "assistant",
            "tool",
            "tool",
            "user",
            "assistant",
            "tool",
            "user",
        ]
        # tool 消息只留文本（字符串形态，原样）
        assert [wire[2]["content"], wire[3]["content"], wire[6]["content"]] == [
            "res-1",
            "res-2",
            "res-3",
        ]
        # 第一轮图片：排在 c1/c2 全部 tool 消息之后，一条 user 消息，顺序 == tool 顺序
        assert content_parts(wire[4])[0]["text"] == FOLLOWUP_GUIDE_TEXT
        assert [b64_of(u) for u in image_urls(wire[4])] == [d1, d2]
        # 第二轮图片：同样排在最后一条 tool 消息之后
        assert [b64_of(u) for u in image_urls(wire[7])] == [d3]

    @pytest.mark.asyncio
    async def test_dropped_image_appends_placeholder_text_part(self):
        """vision=False：无 image part；原文本 part 保留，其后追加占位文本。"""
        data = png_bytes(4, 4)
        ref = make_ref(data)
        msgs = [
            Message(role="tool", tool_call_id="c1", content="envelope", media=[ref]),
        ]
        body = await openai_body(msgs, model=TEXT_MODEL, media=make_media(data))
        parts = content_parts(body["messages"][0])
        assert [p["type"] for p in parts] == ["text", "text"]
        assert parts[0]["text"] == "envelope"
        assert parts[1]["text"] == NO_VISION_PLACEHOLDER
        assert "image_url" not in json.dumps(body)

    @pytest.mark.asyncio
    async def test_image_max_bytes_degrades_oversized(self):
        """超 image_max_bytes → 占位；未设置 / 未超限 → 正常发射（inline 便于逐位断言）。"""
        data = png_bytes(4, 4, b"12345678")
        ref = make_ref(data)
        msgs = [Message(role="tool", tool_call_id="c1", content="env", media=[ref])]
        media = make_media(data)

        capped = await openai_body(
            msgs,
            cfg=openai_cfg(image_delivery="inline", image_max_bytes=len(data) - 1),
            media=media,
        )
        assert image_urls(capped["messages"][0]) == []
        assert content_parts(capped["messages"][0])[-1]["text"] == TOO_LARGE_PLACEHOLDER

        exact = await openai_body(
            msgs,
            cfg=openai_cfg(image_delivery="inline", image_max_bytes=len(data)),
            media=media,
        )
        assert len(image_urls(exact["messages"][0])) == 1

        unset = await openai_body(
            msgs, cfg=openai_cfg(image_delivery="inline"), media=media
        )
        assert len(image_urls(unset["messages"][0])) == 1

    @pytest.mark.asyncio
    async def test_eviction_keeps_newest_and_matches_plan(self, monkeypatch):
        """高水位驱逐：小 policy → 只有最新 K 张是图，其余占位；决策与投影一致。"""
        datas = [png_bytes(4, 4, bytes([i])) for i in range(3)]
        refs = [make_ref(d) for d in datas]
        policy = MediaPolicy(max_images=2, count_quantum=1)
        monkeypatch.setattr("wing.provider.media.resolve_media_policy", lambda: policy)
        msgs = [
            Message(
                role="tool", tool_call_id=f"c{i}", content=f"env-{i}", media=[refs[i]]
            )
            for i in range(3)
        ]
        body = await openai_body(
            msgs, cfg=openai_cfg(image_delivery="inline"), media=make_media(*datas)
        )

        # 最旧一张变占位，最新两张仍是图
        assert image_urls(body["messages"][0]) == []
        assert content_parts(body["messages"][0])[-1]["text"] == BUDGET_PLACEHOLDER
        assert [b64_of(u) for u in image_urls(body["messages"][1])] == [datas[1]]
        assert [b64_of(u) for u in image_urls(body["messages"][2])] == [datas[2]]

        # 与 wing.media 的确定性投影完全一致（不另写判定）
        expected = plan_request_media(msgs, policy=policy, vision=True)
        assert [p.kept for p in expected] == [False, True, True]

    @pytest.mark.asyncio
    async def test_byte_budget_eviction(self, monkeypatch):
        """字节高水位：按量子向上取整丢最旧（encoded_len = 4*ceil(n/3)）。"""
        datas = [sized_png(96), sized_png(96)]
        assert [len(d) for d in datas] == [96, 96]  # encoded_len = 128 各一张
        refs = [make_ref(d) for d in datas]
        policy = MediaPolicy(
            max_images=100,
            request_budget_bytes=200,
            evict_quantum_bytes=64,
        )
        monkeypatch.setattr("wing.provider.media.resolve_media_policy", lambda: policy)
        msgs = [
            Message(role="tool", tool_call_id=f"c{i}", content="env", media=[refs[i]])
            for i in range(2)
        ]
        body = await openai_body(
            msgs, cfg=openai_cfg(image_delivery="inline"), media=make_media(*datas)
        )
        # 两张编码各 128 字节，合计超 200 → 丢最旧一张（量子 64 向上取整）
        assert image_urls(body["messages"][0]) == []
        assert len(image_urls(body["messages"][1])) == 1

    @pytest.mark.asyncio
    async def test_model_vision_switch_downgrades_images(self):
        """换模型降级：同一消息列表，vision 模型发图、text 模型发占位。"""
        data = png_bytes(4, 4, b"switch")
        ref = make_ref(data)
        msgs = [
            Message(
                role="tool",
                tool_call_id="c1",
                content="env",
                media=[ref],
            )
        ]
        media = make_media(data)
        inline = openai_cfg(image_delivery="inline")
        vision = await openai_body(msgs, cfg=inline, model=VISION_MODEL, media=media)
        text = await openai_body(msgs, cfg=inline, model=TEXT_MODEL, media=media)
        assert len(image_urls(vision["messages"][0])) == 1
        assert image_urls(text["messages"][0]) == []
        assert NO_VISION_PLACEHOLDER in json.dumps(text)

    @pytest.mark.asyncio
    async def test_missing_bytes_degrade_to_placeholder(self, caplog_log):
        """字节缺失（media=None / read 返回 None）→ 占位 + WARN，绝不抛。"""
        data = png_bytes(4, 4)
        ref = make_ref(data)
        msgs = [Message(role="tool", tool_call_id="c1", content="env", media=[ref])]

        no_pool = await openai_body(msgs, media=None)
        assert image_urls(no_pool["messages"][0]) == []
        assert content_parts(no_pool["messages"][0])[-1]["text"] == (
            UNAVAILABLE_PLACEHOLDER
        )

        empty_pool = await openai_body(msgs, media=make_media())  # 存储里没有该 id
        assert image_urls(empty_pool["messages"][0]) == []
        assert content_parts(empty_pool["messages"][0])[-1]["text"] == (
            UNAVAILABLE_PLACEHOLDER
        )
        assert len(caplog_log.warnings) >= 2

    @pytest.mark.asyncio
    async def test_media_read_exception_degrades_to_placeholder(self, caplog_log):
        """read 抛异常同样降级为占位 + WARN（存储故障不得打断整次模型调用）。"""
        data = png_bytes(4, 4)
        ref = make_ref(data)
        pool: dict[str, bytes] = {}

        def _boom(mid: str) -> bytes | None:
            raise RuntimeError("disk on fire")

        media = MediaAccess(read=_boom, write=pool.__setitem__)
        msgs = [Message(role="tool", tool_call_id="c1", content="env", media=[ref])]
        body = await openai_body(msgs, media=media)
        assert content_parts(body["messages"][0])[-1]["text"] == (
            UNAVAILABLE_PLACEHOLDER
        )
        assert caplog_log.warnings

    @pytest.mark.asyncio
    async def test_same_media_id_encoded_once_per_request(self):
        """同一 media id 在同一请求内只读/编码一次（per-request 缓存）。"""
        data = png_bytes(4, 4, b"dup")
        ref = make_ref(data)
        reads: list[str] = []
        pool: dict[str, bytes] = {ref.id: data}

        def _read(mid: str) -> bytes | None:
            reads.append(mid)
            return pool.get(mid)

        media = MediaAccess(read=_read, write=pool.__setitem__)
        msgs = [
            Message(role="tool", tool_call_id="c1", content="e1", media=[ref]),
            Message(role="tool", tool_call_id="c2", content="e2", media=[ref]),
        ]
        body = await openai_body(
            msgs, cfg=openai_cfg(image_delivery="inline"), media=media
        )
        assert b64_of(image_urls(body["messages"][0])[0]) == data
        assert b64_of(image_urls(body["messages"][1])[0]) == data
        assert reads == [ref.id]

    @pytest.mark.asyncio
    async def test_mixed_kept_and_dropped_keep_appearance_order(self):
        """同一消息多条媒体：按出现序发射（image / 占位交错），原文本 part 不动。"""
        small, big = sized_png(96), sized_png(200)
        msgs = [
            Message(
                role="tool",
                tool_call_id="c1",
                content="env",
                media=[make_ref(small), make_ref(big)],
            )
        ]
        body = await openai_body(
            msgs,
            cfg=openai_cfg(image_delivery="inline", image_max_bytes=96),
            media=make_media(small, big),
        )
        parts = content_parts(body["messages"][0])
        assert [p["type"] for p in parts] == ["text", "image_url", "text"]
        assert parts[0]["text"] == "env"
        assert b64_of(parts[1]["image_url"]["url"]) == small
        assert parts[2]["text"] == TOO_LARGE_PLACEHOLDER

    @pytest.mark.asyncio
    async def test_followup_images_precede_a_trailing_user_message(self):
        """tool 段后的 steer user 消息：图片消息插在段后、steer 之前。"""
        data = png_bytes(4, 4, b"steer")
        msgs = [
            Message(
                role="tool", tool_call_id="c1", content="res", media=[make_ref(data)]
            ),
            Message(role="user", content="steer!"),
        ]
        body = await openai_body(msgs, media=make_media(data))
        wire = body["messages"]
        assert [m["role"] for m in wire] == ["tool", "user", "user"]
        assert b64_of(image_urls(wire[1])[0]) == data
        assert wire[2]["content"] == "steer!"

    @pytest.mark.asyncio
    async def test_no_media_messages_keep_legacy_shape(self):
        """无媒体快路径：请求体与逐条 to_openai() 完全一致（含 tool_call_id 等）。"""
        msgs = [
            Message(role="system", content="s"),
            Message(role="user", content="q"),
            Message(
                role="assistant",
                content_blocks=[ToolUseBlock(id="c1", name="Bash", input={"a": 1})],
            ),
            Message(role="tool", tool_call_id="c1", content="ok"),
        ]
        body = await openai_body(msgs)
        assert body["messages"] == [m.to_openai() for m in msgs]

    @pytest.mark.asyncio
    async def test_cache_control_skips_image_part(self):
        """explicit_cache_mode：最后 part 是图片时 cache_control 落在前面的 text part。"""
        data = png_bytes(4, 4, b"cache")
        ref = make_ref(data)
        msgs = [
            Message(role="tool", tool_call_id="c1", content="env", media=[ref]),
        ]
        body = await openai_body(
            msgs,
            cfg=openai_cfg(image_delivery="inline", explicit_cache_mode=True),
            media=make_media(data),
        )
        parts = content_parts(body["messages"][0])
        assert parts[0]["cache_control"] == {"type": "ephemeral"}
        assert "cache_control" not in parts[1]

    @pytest.mark.asyncio
    async def test_cache_control_marks_last_part_not_first_non_image(self):
        """explicit_cache_mode 落点规则（S1 回归）：恒为「最后一个 part；仅当它
        是图片时回退到其前面最后一个非图片 part」——从头部正向扫描会把标记
        提前到第一个非图片 part（缓存前缀变短）。
        """
        small, big = sized_png(96), sized_png(200)
        small_ref, big_ref = make_ref(small), make_ref(big)
        pool = make_media(small, big)

        async def _parts(
            msg: Message, *, model: str = VISION_MODEL, cap: int | None = None
        ) -> list[dict]:
            body = await openai_body(
                [msg],
                cfg=openai_cfg(
                    image_delivery="inline",
                    explicit_cache_mode=True,
                    image_max_bytes=cap,
                ),
                model=model,
                media=pool,
            )
            return content_parts(body["messages"][0])

        def _marked(parts: list[dict]) -> int | None:
            marks = [i for i, p in enumerate(parts) if "cache_control" in p]
            assert len(marks) <= 1, "同一消息最多一个 cache_control 标记"
            return marks[0] if marks else None

        # 例1 [text, placeholder]：最后 part 是文本 → 标 index 1（正向扫描会错标 0）
        parts = await _parts(
            Message(role="tool", tool_call_id="c1", content="env", media=[small_ref]),
            model=TEXT_MODEL,
        )
        assert [p["type"] for p in parts] == ["text", "text"]
        assert _marked(parts) == 1

        # 例2 [text, image, placeholder]：最后 part 是文本 → 标 index 2（正向扫描会错标 0）
        parts = await _parts(
            Message(
                role="tool",
                tool_call_id="c1",
                content="env",
                media=[small_ref, big_ref],
            ),
            cap=96,
        )
        assert [p["type"] for p in parts] == ["text", "image_url", "text"]
        assert _marked(parts) == 2

        # 例3 [text, image]：最后 part 是图片 → 回退到前面最后一个非图片 part（index 0）
        parts = await _parts(
            Message(role="tool", tool_call_id="c1", content="env", media=[small_ref])
        )
        assert [p["type"] for p in parts] == ["text", "image_url"]
        assert _marked(parts) == 0

        # 例4 纯图消息（空原文 + 仅保留图片）：无任何非图片 part → 不标记
        parts = await _parts(
            Message(role="tool", tool_call_id="c1", content="", media=[small_ref])
        )
        assert [p["type"] for p in parts] == ["image_url"]
        assert _marked(parts) is None

    @pytest.mark.asyncio
    async def test_cache_control_followup_user_message(self):
        """followup：cache_control 落在图片 user 消息的引导文本上，而非图片。"""
        data = png_bytes(4, 4, b"cache2")
        ref = make_ref(data)
        msgs = [Message(role="tool", tool_call_id="c1", content="env", media=[ref])]
        body = await openai_body(
            msgs,
            cfg=openai_cfg(explicit_cache_mode=True),
            media=make_media(data),
        )
        parts = content_parts(body["messages"][1])
        assert parts[0]["text"] == FOLLOWUP_GUIDE_TEXT
        assert parts[0]["cache_control"] == {"type": "ephemeral"}
        assert "cache_control" not in parts[1]


########## Anthropic 线格式


class TestAnthropicMediaSerialization:
    @pytest.mark.asyncio
    async def test_inline_tool_result_embeds_image_block(self):
        """inline（anthropic 默认）：tool_result content = [text, image]（base64 source）。"""
        data = png_bytes(4, 4, b"anthropic")
        ref = make_ref(data)
        msgs = [
            Message(role="user", content="q"),
            Message(
                role="assistant",
                content_blocks=[ToolUseBlock(id="c1", name="ReadImage", input={})],
            ),
            Message(
                role="tool",
                tool_call_id="c1",
                content="[image: /tmp/a.png | png 4x4]",
                media=[ref],
            ),
        ]
        _, am = await anthropic_messages(msgs, media=make_media(data))
        assert [m["role"] for m in am] == ["user", "assistant", "user"]
        tool_result = am[2]["content"][0]
        assert tool_result["type"] == "tool_result"
        assert tool_result["tool_use_id"] == "c1"
        inner = tool_result["content"]
        assert [b["type"] for b in inner] == ["text", "image"]
        assert inner[0]["text"] == "[image: /tmp/a.png | png 4x4]"
        assert inner[1]["source"]["type"] == "base64"
        assert inner[1]["source"]["media_type"] == "image/png"
        assert base64.b64decode(inner[1]["source"]["data"]) == data

    @pytest.mark.asyncio
    async def test_followup_attaches_images_after_tool_results(self):
        """followup：tool_result 只含文本；image block 追加进段后 user 消息。

        多 tool call 时图片排在全部 tool_result 之后，不与 tool_result 交错。
        """
        d1, d2 = png_bytes(4, 4, b"f1"), png_bytes(4, 4, b"f2")
        r1, r2 = make_ref(d1), make_ref(d2)
        msgs = [
            Message(role="user", content="q"),
            Message(
                role="assistant",
                content_blocks=[
                    ToolUseBlock(id="c1", name="ReadImage", input={}),
                    ToolUseBlock(id="c2", name="ReadImage", input={}),
                ],
            ),
            Message(role="tool", tool_call_id="c1", content="res-1", media=[r1]),
            Message(role="tool", tool_call_id="c2", content="res-2", media=[r2]),
        ]
        _, am = await anthropic_messages(
            msgs,
            cfg=anthropic_cfg(image_delivery="followup"),
            media=make_media(d1, d2),
        )
        assert [m["role"] for m in am] == ["user", "assistant", "user"]
        blocks = am[2]["content"]
        assert [b["type"] for b in blocks] == [
            "tool_result",
            "tool_result",
            "text",
            "image",
            "image",
        ]
        assert blocks[0]["content"] == "res-1"  # tool_result 内只留文本
        assert blocks[1]["content"] == "res-2"
        assert blocks[2]["text"] == FOLLOWUP_GUIDE_TEXT
        assert [base64.b64decode(b["source"]["data"]) for b in blocks[3:]] == [d1, d2]

    @pytest.mark.asyncio
    async def test_inline_multi_tool_images_stay_in_their_tool_result(self):
        """inline：每条 tool_result 各带自己的图片（顺序 == 消息顺序）。"""
        d1, d2 = png_bytes(4, 4, b"i1"), png_bytes(4, 4, b"i2")
        r1, r2 = make_ref(d1), make_ref(d2)
        msgs = [
            Message(role="user", content="q"),
            Message(
                role="assistant",
                content_blocks=[
                    ToolUseBlock(id="c1", name="ReadImage", input={}),
                    ToolUseBlock(id="c2", name="ReadImage", input={}),
                ],
            ),
            Message(role="tool", tool_call_id="c1", content="res-1", media=[r1]),
            Message(role="tool", tool_call_id="c2", content="res-2", media=[r2]),
        ]
        _, am = await anthropic_messages(msgs, media=make_media(d1, d2))
        # tool 消息序列化成 user；两条连续 user 合并为一条
        blocks = am[-1]["content"]
        assert [b["type"] for b in blocks] == ["tool_result", "tool_result"]
        first_inner = blocks[0]["content"]
        second_inner = blocks[1]["content"]
        assert [b["type"] for b in first_inner] == ["text", "image"]
        assert base64.b64decode(first_inner[1]["source"]["data"]) == d1
        assert base64.b64decode(second_inner[1]["source"]["data"]) == d2

    @pytest.mark.asyncio
    async def test_vision_false_appends_placeholder_block(self):
        """vision=False：无 image block，原文本 block 后追加占位 text block。"""
        data = png_bytes(4, 4)
        ref = make_ref(data)
        msgs = [Message(role="tool", tool_call_id="c1", content="env", media=[ref])]
        _, am = await anthropic_messages(msgs, model=TEXT_MODEL, media=make_media(data))
        inner = am[0]["content"][0]["content"]
        assert [b["type"] for b in inner] == ["text", "text"]
        assert inner[0]["text"] == "env"
        assert inner[1]["text"] == NO_VISION_PLACEHOLDER
        assert '"type": "image"' not in json.dumps(am)
        assert '"type": "image"' not in json.dumps(inner)

    @pytest.mark.asyncio
    async def test_eviction_matches_plan(self, monkeypatch):
        """高水位驱逐（anthropic followup）：只有最新 K 张进 followup user 消息。"""
        datas = [png_bytes(4, 4, bytes([i])) for i in range(3)]
        refs = [make_ref(d) for d in datas]
        policy = MediaPolicy(max_images=1, count_quantum=1)
        monkeypatch.setattr("wing.provider.media.resolve_media_policy", lambda: policy)
        msgs = [
            Message(
                role="tool", tool_call_id=f"c{i}", content=f"env-{i}", media=[refs[i]]
            )
            for i in range(3)
        ]
        _, am = await anthropic_messages(
            msgs, cfg=anthropic_cfg(image_delivery="followup"), media=make_media(*datas)
        )
        blocks = am[-1]["content"]
        # 外层：三条 tool_result 后跟 followup 引导文本 + 唯一保留的图片
        assert [b["type"] for b in blocks] == [
            "tool_result",
            "tool_result",
            "tool_result",
            "text",
            "image",
        ]
        # 最旧 / 中间两张被驱逐 → 占位 text block 追加在各自 tool_result 内
        for idx in (0, 1):
            inner = blocks[idx]["content"]
            assert [b["type"] for b in inner] == ["text", "text"]
            assert inner[1]["text"] == BUDGET_PLACEHOLDER
        assert blocks[2]["content"] == "env-2"  # 保留图搬走 → 只留文本（字符串）
        assert blocks[3]["text"] == FOLLOWUP_GUIDE_TEXT
        assert base64.b64decode(blocks[4]["source"]["data"]) == datas[2]

    @pytest.mark.asyncio
    async def test_missing_bytes_degrade_to_placeholder(self, caplog_log):
        """字节缺失：tool_result 追加 UNAVAILABLE 占位 block + WARN，不抛。"""
        data = png_bytes(4, 4)
        ref = make_ref(data)
        msgs = [Message(role="tool", tool_call_id="c1", content="env", media=[ref])]
        _, am = await anthropic_messages(msgs, media=None)
        inner = am[0]["content"][0]["content"]
        assert [b["type"] for b in inner] == ["text", "text"]
        assert inner[1]["text"] == UNAVAILABLE_PLACEHOLDER
        assert caplog_log.warnings

    @pytest.mark.asyncio
    async def test_no_media_messages_keep_legacy_shape(self):
        """无媒体快路径：tool_result 的 content 仍是字符串（既有形态）。"""
        msgs = [
            Message(role="user", content="q"),
            Message(
                role="assistant",
                content_blocks=[ToolUseBlock(id="c1", name="Bash", input={})],
            ),
            Message(role="tool", tool_call_id="c1", content="ok"),
        ]
        _, am = await anthropic_messages(msgs)
        assert am[2]["content"][0]["content"] == "ok"

    @pytest.mark.asyncio
    async def test_cache_control_skips_image_block(self):
        """explicit_cache_mode + followup：cache_control 落在引导文本 block 上。"""
        data = png_bytes(4, 4, b"cache-a")
        ref = make_ref(data)
        msgs = [Message(role="tool", tool_call_id="c1", content="env", media=[ref])]
        p = AnthropicProvider(
            anthropic_cfg(image_delivery="followup", explicit_cache_mode=True),
            media=make_media(data),
        )
        try:
            body = p._build_body(msgs, VISION_MODEL, None, False)
        finally:
            await p.aclose()
        blocks = body["messages"][-1]["content"]
        guide = blocks[-2]
        assert guide["type"] == "text" and guide["text"] == FOLLOWUP_GUIDE_TEXT
        assert guide["cache_control"] == {"type": "ephemeral"}
        assert "cache_control" not in blocks[-1]


########## plan_for_request 直测


class TestPlanForRequest:
    def test_uses_request_model_not_provider_default(self):
        """能力解析以本次请求的 model 参数为准（运行时切换模型即生效）。"""
        cfg = openai_cfg()
        data = png_bytes(4, 4)
        msgs = [
            Message(role="tool", tool_call_id="c1", content="e", media=[make_ref(data)])
        ]
        assert (
            plan_for_request(msgs, provider_cfg=cfg, model=VISION_MODEL)[0].kept is True
        )
        assert (
            plan_for_request(msgs, provider_cfg=cfg, model=TEXT_MODEL)[0].kept is False
        )

    def test_injected_policy_drives_decisions(self):
        """显式注入 policy 时无需读全局配置，决策与 plan_request_media 全等。"""
        data = [png_bytes(4, 4, bytes([i])) for i in range(3)]
        msgs = [
            Message(role="tool", tool_call_id=f"c{i}", content="e", media=[make_ref(d)])
            for i, d in enumerate(data)
        ]
        policy = MediaPolicy(max_images=1, count_quantum=1)
        plans = plan_for_request(
            msgs, provider_cfg=openai_cfg(), model=VISION_MODEL, policy=policy
        )
        assert [p.kept for p in plans] == [False, False, True]

    def test_system_message_media_is_not_projected(self):
        """system 消息的 media 不产生任何 plan：既不保留，也不占位（两协议共同语义）。"""
        data = png_bytes(4, 4, b"sys")
        msgs = [
            Message(role="system", content="s", media=[make_ref(data)]),
            Message(role="user", content="q"),
        ]
        assert (
            plan_for_request(msgs, provider_cfg=openai_cfg(), model=VISION_MODEL) == []
        )


########## system 消息的 media：两协议统一忽略


class TestSystemMessageMediaIgnored:
    """`Message.media` 挂在 system 消息上时两协议行为对齐：不发图、也不加占位（N1）。

    当前不可达（ReadImage 只写 tool 消息，`Message.media` 注明「仅 tool，预留
    user」）——本组测试是未来扩展（user 贴图等）的对齐锚点：任何协议都不得把
    image part / 占位文本发射进 system 段。
    """

    @staticmethod
    def _messages(data: bytes) -> list[Message]:
        return [
            Message(role="system", content="sys", media=[make_ref(data)]),
            Message(role="user", content="q"),
            Message(role="assistant", content="a"),
        ]

    @pytest.mark.asyncio
    async def test_openai_system_message_carries_no_image_or_placeholder(self):
        """openai：system content 仍是裸字符串——无 image part、无占位。"""
        data = png_bytes(4, 4, b"sys-openai")
        body = await openai_body(self._messages(data), media=make_media(data))
        assert body["messages"][0] == {"role": "system", "content": "sys"}
        wire = json.dumps(body)
        assert "image_url" not in wire
        for placeholder in (
            NO_VISION_PLACEHOLDER,
            TOO_LARGE_PLACEHOLDER,
            BUDGET_PLACEHOLDER,
            UNAVAILABLE_PLACEHOLDER,
        ):
            assert placeholder not in wire

    @pytest.mark.asyncio
    async def test_anthropic_system_text_carries_no_image_or_placeholder(self):
        """anthropic：system 段只取文本——无 image block、无占位 block。"""
        data = png_bytes(4, 4, b"sys-anthropic")
        system_text, am = await anthropic_messages(
            self._messages(data), media=make_media(data)
        )
        assert system_text == "sys"
        wire = json.dumps(am)
        assert '"type": "image"' not in wire
        for placeholder in (
            NO_VISION_PLACEHOLDER,
            TOO_LARGE_PLACEHOLDER,
            BUDGET_PLACEHOLDER,
            UNAVAILABLE_PLACEHOLDER,
        ):
            assert placeholder not in wire


########## 压缩剥离


class _CapturingProvider:
    """假 provider：记下 do_compact 发出的消息列表，返回合法摘要。"""

    def __init__(self) -> None:
        self.messages: list[Message] | None = None

    async def generate(
        self,
        messages: list[Message],
        model: str,
        tools: list | None = None,
        stream: bool = False,
        accumulator: Any = None,
    ):
        self.messages = messages
        yield LLMResponse(content="<summary>ok</summary>")


class TestCompactStripsMedia:
    @pytest.mark.asyncio
    async def test_compact_request_has_no_media_and_does_not_mutate_chain(self):
        data = png_bytes(4, 4, b"compact")
        ref = make_ref(data)
        full = [
            Message(role="user", content="q"),
            Message(role="tool", tool_call_id="c1", content="env", media=[ref]),
        ]
        provider = _CapturingProvider()
        c = Compactor(context_window_tokens=100_000, keep_recent_tokens=20_000)
        await c.do_compact(
            full,
            VISION_MODEL,
            provider,  # ty: ignore[invalid-argument-type]
        )

        assert provider.messages is not None
        assert len(provider.messages) == len(full) + 1
        assert all(m.media is None for m in provider.messages)
        assert provider.messages[1].content == "env"  # 文本保留
        # 原链消息零改动（浅拷贝，不污染）
        assert full[1].media is not None and full[1].media[0].id == ref.id

        # 端到端投影：压缩请求即使走真实 provider 序列化也不含 image part
        body = await openai_body(provider.messages, media=make_media(data))
        assert "image_url" not in json.dumps(body)


########## fixture：media 模块 logger 替身（log.propagate=False，caplog 收不到）


class _RecordingLogger:
    def __init__(self) -> None:
        self.warnings: list[str] = []

    def warning(self, msg: str) -> None:
        self.warnings.append(msg)


@pytest.fixture
def caplog_log(monkeypatch: pytest.MonkeyPatch) -> _RecordingLogger:
    rec = _RecordingLogger()
    monkeypatch.setattr("wing.provider.media.log", rec)
    return rec
