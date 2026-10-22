# wing/provider/anthropic/serialize.py
"""Anthropic 请求期消息序列化：Message 列表 → Anthropic Messages 格式。

含请求期媒体投影（inline / followup 两种线格式与占位追加）与 cache_control
标记；方法经 ``_SerializeMixin`` 与 ``AnthropicProvider`` 合体（见 ``provider``）。
"""

from __future__ import annotations

from collections.abc import Callable
from typing import TYPE_CHECKING, Any

from wing.provider.base import mark_last_cache_control
from wing.provider.media import (
    FOLLOWUP_GUIDE_TEXT,
    group_plans_by_message,
    message_slots,
    plan_for_request,
    resolve_image_delivery,
)
from wing.schema import (
    CachePart,
    MediaRef,
    Message,
    TextBlock,
    ThinkingBlock,
    Tool,
    ToolUseBlock,
)
from wing.schema.cache import history_label

if TYPE_CHECKING:
    from wing.config import ProviderConfig
    from wing.media import MediaPlan
    from wing.provider.base import RequestOptions

_IMAGE_PART_TYPES: frozenset[str] = frozenset({"image"})
"""cache_control 不落在图片 block 上——本协议的图片 block 类型名。"""


class _SerializeMixin:
    """``AnthropicProvider`` 的请求期序列化方法组（与 provider 类合体后生效）。"""

    _config: ProviderConfig

    probe_part: Callable[[str, Any], "CachePart | None"]
    """由 ``ModelProvider`` 提供（分段构造的永不抛边界，见 base）。"""

    def _serialize_messages(
        self,
        messages: list[Message],
        model: str,
        options: "RequestOptions | None" = None,
        parts: list[CachePart] | None = None,
    ) -> tuple[str, list[dict]]:
        """将 Message 列表转换为 Anthropic 格式（含请求期媒体投影）。

        会话媒体读接口经 ``options.media`` 注入（会话级；None = 无存储）。

        无媒体的消息与引入媒体前逐字节一致（快路径不触碰全局配置）。有媒体时：

        - ``inline``（anthropic 协议默认）：图片以 ``image`` block 内嵌进该
          tool_result 的 content 数组（base64 source）；
        - ``followup``：tool_result 只含文本，图片以 image block 追加进**连续
          tool 消息段之后**的 user 消息（复用 _merge_consecutive_messages 的
          合并路径——合并只 extend 外层 content 数组）——图片永远排在全部
          tool 消息之后，不会插在两条 tool 消息之间。

        被丢弃 / 字节缺失的图片位：不改原文本 block，只在其后追加一个占位
        text block；缺文本块不发射（Anthropic 拒绝空 text block）。

        ``parts`` 非 None 时顺带收集缓存探针的 ``history:<uuid>:<role>`` 分段
        （每条产生协议 dict 的非 system 消息一段）。hash 取**合并 / cache_control
        之前**的形态：合并与 cache_control 都在本次请求的末尾动手，把它的移动
        算进 hash 会让内容没变的相邻两次调用在末段对不上（假 prefix_broken）。

        Returns:
            (system_text, anthropic_messages)
        """
        system_parts: list[str] = []
        anthropic_msgs: list[dict] = []

        delivery = resolve_image_delivery(self._config)
        plans: dict[int, list[MediaPlan]] = {}
        cache: dict[str, str | None] = {}
        if any(m.media for m in messages):
            plans = group_plans_by_message(
                plan_for_request(messages, provider_cfg=self._config, model=model)
            )
        pending_images: list[dict] = []  # followup：待追加的 image blocks

        def flush_pending() -> None:
            """把当前连续 tool 段积累的图片落到一条 user 消息（段后位置）。"""
            if not pending_images:
                return
            anthropic_msgs.append(
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": FOLLOWUP_GUIDE_TEXT},
                        *pending_images,
                    ],
                }
            )
            pending_images.clear()

        for i, msg in enumerate(messages):
            if msg.role == "system":
                # system 段只承载文本：其 media 被投影层忽略（不发图也不加占位），
                # 与 openai 路径行为一致——见 wing.media.plan_request_media。
                if msg.content:
                    system_parts.append(msg.content)
                continue

            if delivery == "followup" and msg.role != "tool":
                # 连续 tool 段结束——图片挂段后（即当前消息之前）。
                flush_pending()

            slots = message_slots(
                plans.get(i, []),
                media=options.media if options is not None else None,
                cache=cache,
                role=msg.role,
            )

            if msg.role == "assistant":
                blocks = self._serialize_assistant(msg)
                # 零块 assistant（存量历史的空记录 / thinking 块被剥离的产物）
                # MUST NOT 发出 content: []——Anthropic 要求 content 至少一个块，
                # 否则本次及该 session 后续所有请求 400。丢弃是配对安全的：
                # 零块即无 tool_use，不会有后续 tool_result 引用本条。
                if not blocks:
                    continue
                wire = {"role": "assistant", "content": blocks}
                anthropic_msgs.append(wire)
                # hash 在这一刻定型（合并 / cache_control 都还没动 wire）
                if parts is not None:
                    part = self.probe_part(history_label(msg.uuid, "assistant"), wire)
                    if part is not None:
                        parts.append(part)

            elif msg.role == "tool":
                content: str | list[dict] = msg.content or ""
                extra: list[dict] = []
                for slot in slots:
                    if slot.b64 is None:
                        extra.append({"type": "text", "text": slot.placeholder})
                    elif delivery == "followup":
                        # 保留图片搬到段后 user 消息——tool_result 只留文本。
                        pending_images.append(self._image_block(slot.ref, slot.b64))
                    else:
                        extra.append(self._image_block(slot.ref, slot.b64))
                if extra:
                    # 数组化：原文本保持为独立 text block（一个字节都不改），
                    # 追加物随后——空原文不发射空 text block。
                    content = (
                        [{"type": "text", "text": msg.content}] if msg.content else []
                    ) + extra
                wire = {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": msg.tool_call_id or "",
                            "content": content,
                        }
                    ],
                }
                anthropic_msgs.append(wire)
                if parts is not None:
                    part = self.probe_part(history_label(msg.uuid, "tool"), wire)
                    if part is not None:
                        parts.append(part)

            elif msg.role == "user":
                blocks: list[dict] = []
                if msg.content:
                    blocks.append({"type": "text", "text": msg.content})
                # user 贴图的媒体位（无"轮"可挂靠，就地发射）——纯图消息
                # （content 为空）只发射 image block。
                for slot in slots:
                    if slot.b64 is None:
                        blocks.append({"type": "text", "text": slot.placeholder})
                    else:
                        blocks.append(self._image_block(slot.ref, slot.b64))
                if blocks:
                    wire = {"role": "user", "content": blocks}
                    anthropic_msgs.append(wire)
                    if parts is not None:
                        part = self.probe_part(history_label(msg.uuid, "user"), wire)
                        if part is not None:
                            parts.append(part)

        flush_pending()
        # 合并连续同角色消息：Anthropic Messages API 要求 user/assistant
        # 严格交替，连续同角色返回 400。两个真实产生路径：tool 消息序列化为
        # user（tool_result）后紧跟 steer 注入的 user 消息；无效轮次重试续跑
        # （截断轮的 content 与重试轮的 tool call 相邻两条 assistant）。
        merged = self._merge_consecutive_messages(anthropic_msgs)
        return "\n\n".join(system_parts), merged

    @staticmethod
    def _image_block(ref: MediaRef, b64: str) -> dict:
        """图片位 → Anthropic image block（base64 source）。"""
        return {
            "type": "image",
            "source": {"type": "base64", "media_type": ref.mime, "data": b64},
        }

    def _serialize_assistant(self, msg: Message) -> list[dict]:
        """将 assistant 消息的 content_blocks 按序一对一映射回 Anthropic block。

        忠实回放契约：
        - thinking 有 signature → 原样按序发出，一个字节都不改
          （改了会签名失配 + 击穿 prompt cache）
        - thinking 无 signature → 原样发空签名（signature:""）。此类数据
          产生于不下发签名的推理 provider 或存量旧会话——回放官方 Anthropic
          被拒是预期行为（官方必下发签名），MUST NOT 降级 text 破坏内容语义
        - thinking 文本为空且无 signature → 整块丢弃
        - redacted → redacted_thinking + data（不透明黑盒原样回放）
        """
        blocks: list[dict] = []
        for block in msg.content_blocks or []:
            if isinstance(block, TextBlock):
                if block.text:
                    blocks.append({"type": "text", "text": block.text})
            elif isinstance(block, ThinkingBlock):
                if block.redacted:
                    blocks.append(
                        {"type": "redacted_thinking", "data": block.signature or ""}
                    )
                elif not block.thinking.strip() and not block.signature:
                    continue
                else:
                    blocks.append(
                        {
                            "type": "thinking",
                            "thinking": block.thinking,
                            "signature": block.signature or "",
                        }
                    )
            elif isinstance(block, ToolUseBlock):
                blocks.append(
                    {
                        "type": "tool_use",
                        "id": block.id,
                        "name": block.name,
                        "input": block.input,
                    }
                )
        return blocks

    @staticmethod
    def _merge_consecutive_messages(msgs: list[dict]) -> list[dict]:
        """合并连续同角色消息（user-user / assistant-assistant）。

        只 extend content 块数组，块内容（含 thinking 签名）原样保留：
        - assistant-assistant：无效轮次重试续跑——截断轮的 content 与重试轮
          的 tool call 是两条相邻 assistant，合并恰好还原「text + tool_use
          同一条」的未截断形态；
        - user-user：tool_result 后紧跟 steer 注入的 user 文本。
        """
        if not msgs:
            return msgs
        merged: list[dict] = [msgs[0]]
        for msg in msgs[1:]:
            prev = merged[-1]
            if msg["role"] == prev["role"]:
                prev["content"].extend(msg["content"])
            else:
                merged.append(msg)
        return merged

    @staticmethod
    def _tool_to_anthropic(tool: Tool) -> dict:
        """将 Tool 转换为 Anthropic tool 定义格式。"""
        openai_def = tool.to_openai()
        func = openai_def["function"]
        return {
            "name": func["name"],
            "description": func.get("description", ""),
            "input_schema": func.get(
                "parameters", {"type": "object", "properties": {}}
            ),
        }

    @staticmethod
    def _apply_cache_control(anthropic_messages: list[dict]) -> None:
        """为最后一条消息的最后一个 content block 打 cache_control。

        与 OpenAI 路径同构（规则见 :func:`wing.provider.base.mark_last_cache_control`）；
        Anthropic prompt caching 支持 thinking 块携带缓存标记，所以这里不改写
        thinking 字节、也不影响签名。最后一个 block 是图片时回退到其前面最后一个
        非图片 block（text / tool_result）——纯图消息时跳过本次标记。
        """
        mark_last_cache_control(anthropic_messages, image_types=_IMAGE_PART_TYPES)
