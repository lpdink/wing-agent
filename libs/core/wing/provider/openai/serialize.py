# wing/provider/openai/serialize.py
"""OpenAI 兼容协议请求期消息序列化：Message 列表 → OpenAI 格式。

含请求期媒体投影（inline / followup 线格式与占位追加）与 cache_control 标记；
方法经 ``_SerializeMixin`` 与 ``OpenAICompatProvider`` 合体（见 ``provider``）。
"""

from __future__ import annotations

from typing import TYPE_CHECKING, cast

from wing.provider.media import (
    FOLLOWUP_GUIDE_TEXT,
    group_plans_by_message,
    message_slots,
    plan_for_request,
    resolve_image_delivery,
)
from wing.schema import MediaRef, Message

if TYPE_CHECKING:
    from wing.config import ProviderConfig
    from wing.media import MediaAccess


class _SerializeMixin:
    """``OpenAICompatProvider`` 的请求期序列化方法组（与 provider 类合体后生效）。"""

    _config: ProviderConfig
    _media: MediaAccess | None

    def _serialize_messages(self, messages: list[Message], model: str) -> list[dict]:
        """序列化请求消息（含请求期媒体投影：能力降级 / 高水位驱逐 / 线格式）。

        整条请求任何消息都无媒体时，直接 ``[m.to_openai() ...]`` 返回——与引入
        媒体前的请求体逐字节一致，且不触碰全局配置。请求内有媒体时按
        ``image_delivery`` 发射（无媒体的邻座消息仍逐条经 ``to_openai()``，
        字节不变）：

        - ``inline``：图片留在原消息的 content 数组里（``image_url`` data URL）；
        - ``followup``（openai 协议默认）：**连续 tool 消息段**内的保留图片
          汇总成一条 user 消息，插在该段之后（即下一个非 tool 消息之前）；
          多 tool call 多图只出一条、顺序 == tool 消息顺序 × 消息内顺序。

        被丢弃 / 字节缺失的图片位：不改原文本块，只在其后追加一个占位文本
        part（保留消息的字节在驱逐前后完全一致 → 前缀 cache 最大复用）。
        """
        if not any(m.media for m in messages):
            return [m.to_openai() for m in messages]

        delivery = resolve_image_delivery(self._config)
        plans = group_plans_by_message(
            plan_for_request(messages, provider_cfg=self._config, model=model)
        )
        cache: dict[str, str | None] = {}
        out: list[dict] = []
        pending: list[dict] = []  # followup：待汇总到段后 user 消息的 image_url parts

        def flush_pending() -> None:
            """把当前连续 tool 段积累的图片落到一条 user 消息（段后位置）。"""
            if not pending:
                return
            out.append(
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": FOLLOWUP_GUIDE_TEXT},
                        *pending,
                    ],
                }
            )
            pending.clear()

        for i, msg in enumerate(messages):
            if delivery == "followup" and msg.role != "tool":
                # 连续 tool 段结束——图片挂段后（即当前消息之前）。
                flush_pending()

            # system 消息的 media 一律忽略（投影层已剔除，见 plan_request_media）：
            # 本协议 system content 只允许文本 part——此处 slots 恒为空，不发图
            # 也不加占位，与 anthropic 路径行为一致。
            slots = message_slots(plans.get(i, []), media=self._media, cache=cache)
            base = msg.to_openai()
            move_kept = delivery == "followup" and msg.role == "tool"
            extra: list[dict] = []
            for slot in slots:
                if slot.b64 is None:
                    extra.append({"type": "text", "text": slot.placeholder})
                elif move_kept:
                    pending.append(self._image_part(slot.ref, slot.b64))
                else:
                    extra.append(self._image_part(slot.ref, slot.b64))
            if extra:
                # content 数组化：原文本保持为独立 part（一个字节都不改），
                # 追加物随后——空原文不发射空 text part。
                base["content"] = (
                    [{"type": "text", "text": base["content"]}]
                    if base["content"]
                    else []
                ) + extra
            out.append(base)

        flush_pending()
        return out

    @staticmethod
    def _image_part(ref: MediaRef, b64: str) -> dict:
        """图片位 → OpenAI content part（data URL，base64 内联）。"""
        return {
            "type": "image_url",
            "image_url": {"url": f"data:{ref.mime};base64,{b64}"},
        }

    # ─── Cache Control ────────────────────────────────────────────

    @staticmethod
    def _apply_cache_control(openai_messages: list[dict]) -> None:
        """为最后一条消息的最后一个 content block 追加 cache_control 标记。

        落点恒为「最后一个 part；仅当它是图片（``image_url``）时回退到其前面
        最后一个非图片 part」——因此从尾部反向扫描（cache_control 是 OpenAI
        兼容网关的非标准扩展字段，不落在图片上；从头部正向扫描会把标记提前到
        第一个非图片 part，缓存前缀变短）；整条消息没有任何非图片 part（纯图
        消息）时跳过本次标记。
        """
        if not openai_messages:
            return
        last_msg = openai_messages[-1]
        content = last_msg.get("content")
        if content is None:
            return
        if isinstance(content, str):
            last_msg["content"] = [
                {
                    "type": "text",
                    "text": content,
                    "cache_control": {"type": "ephemeral"},
                }
            ]
        elif isinstance(content, list):
            for part in reversed(cast("list[dict]", content)):
                if part.get("type") == "image_url":
                    continue
                part["cache_control"] = {"type": "ephemeral"}
                return
