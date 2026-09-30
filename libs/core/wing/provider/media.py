# wing/provider/media.py
"""请求期媒体投影与序列化原语（OpenAI 兼容 / Anthropic 两个协议共用）。

为什么独立成模块（而不是放进 wing/media.py 或 provider/base.py）：
- wing/media.py 是「纯函数、不依赖 config」的地基层（step 01 的边界）——
  本模块要读 provider 配置与全局 ``Config.images``；
- provider/base.py 是协议中立的抽象基类——不应引入全局 config 访问与
  媒体编码/降级逻辑；
- 两个协议实现共同消费投影结果，放在 provider 包内的独立模块既不产生
  openai↔anthropic 横向依赖，也不污染地基。

投影算法本体在 ``wing.media.plan_request_media``（唯一判定来源）：本模块只做
「配置解析 + 消息对齐 + 处置 → 序列化原语」的薄壳，不另写判定。
"""

from __future__ import annotations

import base64
from collections.abc import Sequence
from dataclasses import dataclass
from typing import TYPE_CHECKING, Literal

from wing.common.logger import log
from wing.config import resolve_model_capabilities
from wing.media import (
    PLACEHOLDER_BY_REASON,
    MediaAccess,
    MediaPlan,
    MediaPolicy,
    plan_request_media,
)
from wing.schema import MediaRef, Message

if TYPE_CHECKING:
    from wing.config import ProviderConfig

ImageDelivery = Literal["inline", "followup"]
"""图片投递形态：inline = 留在原消息；followup = 汇总为段后 user 消息。"""

FOLLOWUP_GUIDE_TEXT = "Images read by the preceding tool results are attached below."
"""followup 形态的引导文案（user 消息首块）——冻结字符串，两条协议共用。"""

UNAVAILABLE_PLACEHOLDER = (
    "(image unavailable: stored image bytes could not be read; "
    "re-read the file to attach it again)"
)
"""字节缺失的占位文本（媒体池不可用 / read 返回 None / read 抛异常）。

独立于 wing.media 的三条 dropped 文案：那三条解释「为什么没发这张图」
（能力/预算/尺寸），本条解释「存储侧故障」——混淆文案会把归因引向错误方向。
"""


########## 配置解析


def resolve_image_delivery(provider_cfg: ProviderConfig) -> ImageDelivery:
    """解析图片投递形态：显式配置优先；None = 按协议默认。

    openai → followup（DeepSeek/DashScope 等兼容网关的文档口径都是 user
    消息带图，最宽兼容）；anthropic → inline（tool_result 内嵌 image block，
    官方原生路径）。
    """
    if provider_cfg.image_delivery is not None:
        return provider_cfg.image_delivery
    return "inline" if provider_cfg.protocol == "anthropic" else "followup"


def resolve_media_policy() -> MediaPolicy:
    """从全局 ``Config.images`` 解析请求期保留策略（每请求解析，配置热重载即生效）。"""
    from wing.config import get_config  # 延迟 import：便于测试 patch / 避免环

    images = get_config().images
    return MediaPolicy(
        max_images=images.max_images,
        count_quantum=images.count_quantum,
        request_budget_bytes=images.request_budget_bytes,
        evict_quantum_bytes=images.evict_quantum_bytes,
    )


def plan_for_request(
    messages: Sequence[Message],
    *,
    provider_cfg: ProviderConfig,
    model: str,
    policy: MediaPolicy | None = None,
) -> list[MediaPlan]:
    """请求期图片投影：能力 + 策略 + 消息列表 → 每条 media 出现的处置。

    - vision 以**本次请求的 model 参数**为准（模型可运行时切换）；
    - policy 缺省从全局 Config.images 解析（测试/调用方可显式注入小策略，
      不必改全局配置）；
    - max_image_bytes 取 provider 配置的单图兜底上限。

    返回值按消息序 × 消息内出现序排列，消费方按 ``message_index`` 对齐消息。
    """
    if policy is None:
        policy = resolve_media_policy()
    return plan_request_media(
        messages,
        policy=policy,
        vision=resolve_model_capabilities(provider_cfg, model).vision,
        max_image_bytes=provider_cfg.image_max_bytes,
    )


def group_plans_by_message(plans: Sequence[MediaPlan]) -> dict[int, list[MediaPlan]]:
    """把投影结果按 message_index 分组（保持出现序——序列化按序发射）。"""
    grouped: dict[int, list[MediaPlan]] = {}
    for plan in plans:
        grouped.setdefault(plan.message_index, []).append(plan)
    return grouped


########## 处置 → 序列化原语


@dataclass(frozen=True)
class MediaSlot:
    """一条 media 出现经投影后的序列化原语。

    恰好一个非 None：
    - ``b64`` 非 None → 该位以图片发射（内容为 base64 编码）；
    - ``placeholder`` 非 None → 该位在线上以占位文本出现（被丢弃或字节缺失）。
    """

    ref: MediaRef
    b64: str | None = None
    placeholder: str | None = None


def _read_media_bytes(media: MediaAccess | None, ref: MediaRef) -> bytes | None:
    """读取图片字节；任何缺失/异常路径都 WARN + None，绝不抛（序列化必须完成）。"""
    if media is None:
        log.warning(
            f"media pool unavailable, image {ref.id[:8]} degraded to placeholder"
        )
        return None
    try:
        data = media.read(ref.id)
    except Exception as e:  # noqa: BLE001 —— 存储故障不能打断整次模型调用
        log.warning(f"read media {ref.id[:8]} failed: {e}; degraded to placeholder")
        return None
    if data is None:
        log.warning(f"media {ref.id[:8]} missing from store; degraded to placeholder")
        return None
    return data


def encode_media_ref(
    media: MediaAccess | None, ref: MediaRef, cache: dict[str, str | None]
) -> str | None:
    """读取并 base64 编码一张图片；缺失/异常一律 WARN + None。

    cache 是**每请求一份**的 dict（key = media id）：同一图片在一条请求里
    出现多次只读盘/编码一次；失败结果（None）同样入缓存——存储故障时不
    反复读盘、不刷日志。不做跨请求全局缓存（存储修复后应自然恢复）。
    """
    if ref.id in cache:
        return cache[ref.id]
    data = _read_media_bytes(media, ref)
    encoded = base64.b64encode(data).decode("ascii") if data is not None else None
    cache[ref.id] = encoded
    return encoded


def message_slots(
    msg: Message,
    plans: Sequence[MediaPlan],
    *,
    media: MediaAccess | None,
    cache: dict[str, str | None],
) -> list[MediaSlot]:
    """把一条消息的媒体处置投影成按出现序的 MediaSlot 列表。

    ``plans`` 只含本消息的条目（调用方用 group_plans_by_message 过滤）。
    kept 位读字节编码（失败 → UNAVAILABLE 占位）；dropped 位取 wing.media
    的三条占位常量之一——原文本块由序列化层负责「不动」，本函数只产出追加物。
    """
    slots: list[MediaSlot] = []
    for plan in plans:
        if plan.drop_reason is not None:
            slots.append(
                MediaSlot(
                    ref=plan.ref, placeholder=PLACEHOLDER_BY_REASON[plan.drop_reason]
                )
            )
            continue
        b64 = encode_media_ref(media, plan.ref, cache)
        if b64 is None:
            slots.append(MediaSlot(ref=plan.ref, placeholder=UNAVAILABLE_PLACEHOLDER))
        else:
            slots.append(MediaSlot(ref=plan.ref, b64=b64))
    return slots
