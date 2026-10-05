# wing/media/policy.py
"""请求期图片投影（高水位 + 量子批量驱逐）与占位常量。

投影算法的动机：视觉 token 内联在 token 序列中，任何让图片表示变化的动作都会
**从首个受影响 token 起**打断前缀 cache。所以策略是「常态一张不丢；只有硬阈值
逼到才丢，且按量子批量丢最旧的」，并保证投影是消息列表的确定性函数（无持久
状态、无时间戳）。

只用 MediaRef 元数据做确定性决策；不读字节、不依赖 config。
"""

from __future__ import annotations

from collections.abc import Callable, Sequence
from dataclasses import dataclass
from enum import Enum

from wing.schema import MediaRef, Message

from .facts import encoded_len


########## 投影（请求期图片保留策略）


class DropReason(str, Enum):
    """图片出现被丢弃的原因（消息列表投影的判定结果）。"""

    NO_VISION = "no_vision"
    """当前模型无视觉能力——整条请求的图片位全部降级为占位文本。"""
    TOO_LARGE = "too_large"
    """单图超过 provider 的 image_max_bytes 兜底上限。"""
    BUDGET = "budget"
    """触发计数/字节高水位，被量子批量驱逐（含两种规则取更激进者）。"""


NO_VISION_PLACEHOLDER = "(image omitted: this model does not accept image input)"
BUDGET_PLACEHOLDER = (
    "(image omitted from this request: context image budget; "
    "re-read the file to attach it again)"
)
TOO_LARGE_PLACEHOLDER = "(image omitted: exceeds this provider's per-image size limit)"

PLACEHOLDER_BY_REASON: dict[DropReason, str] = {
    DropReason.NO_VISION: NO_VISION_PLACEHOLDER,
    DropReason.TOO_LARGE: TOO_LARGE_PLACEHOLDER,
    DropReason.BUDGET: BUDGET_PLACEHOLDER,
}
"""被丢弃出现的固定说明文本块——追加在原文本块之后，原文本块不动。"""


@dataclass(frozen=True)
class MediaPolicy:
    """图片保留策略（高水位 + 量子批量驱逐，KV-cache 友好）。

    默认值是任务书冻结值：
    - max_images / count_quantum：计数高水位 32，超出按 8 的倍数从最旧丢；
    - request_budget_bytes / evict_quantum_bytes：base64 编码后累计 36 MiB，
      超出按 18 MiB 的倍数从最旧丢（DeepSeek 48 MiB 请求体上限的 75%）。
    """

    max_images: int = 32
    count_quantum: int = 8
    request_budget_bytes: int = 37_748_736  # 36 MiB，base64 编码后
    evict_quantum_bytes: int = 18_874_368  # 18 MiB


@dataclass(frozen=True)
class MediaPlan:
    """一条 media 出现的处置结果（按出现序，kept 与 dropped 都在列表里）。"""

    message_index: int
    """该出现所属消息在入参 messages 中的下标。"""
    ref: MediaRef
    drop_reason: DropReason | None = None
    """None = kept（进请求）；否则为丢弃原因。"""

    @property
    def kept(self) -> bool:
        """是否保留（未被丢弃）。"""
        return self.drop_reason is None


@dataclass(frozen=True)
class MediaAccess:
    """会话媒体读写窄接口（Session 用 store.read_media/write_media 构造）。

    工具（写图）与 provider 序列化（读图编码）都只经此接口，不直接触碰
    SessionStore——同一实例共享同一存储池。
    """

    read: Callable[[str], bytes | None]
    write: Callable[[str, bytes], None]


def _ceil_div(a: int, b: int) -> int:
    """整数 ceil(a/b)（b > 0）。"""
    return -(-a // b)


def _quantum(value: int) -> int:
    """驱逐量子防御性归一：非正数退化为 1（逐条丢），避免除零。"""
    return value if value > 0 else 1


def plan_request_media(
    messages: Sequence[Message],
    *,
    policy: MediaPolicy,
    vision: bool,
    max_image_bytes: int | None = None,
) -> list[MediaPlan]:
    """请求期图片投影：确定性、无状态、不读字节，丢弃恒为最旧优先。

    ``role == "system"`` 的消息上的 media 一律**不参与投影**（既不发图也不加
    占位）：两个协议都不允许 system 携带图片（openai 的 system content 只允许
    文本 part，anthropic 的 system 段只取文本）——忽略是两协议的共同语义，
    "占位"文案的语义（被丢弃 / 不可用）与之无关。

    算法（严格按任务书，按序）：

    0. ``vision=False`` → 全部 ``no_vision``（模型不读图，工具早已拒绝，
       此处是"会话中切到文本模型"的兜底投影）。
    1. ``max_image_bytes`` 已设置且 ``ref.bytes > cap`` → ``too_large``
       （单图尺寸兜底，严格大于）。
    2. 计数：剩余（eligible）超过 ``max_images`` 时，按 ``count_quantum``
       向上取整得到需丢条数。
    3. 字节：eligible 的 ``Σ encoded_len(bytes)`` 超过
       ``request_budget_bytes`` 时，按 ``evict_quantum_bytes`` 向上取整得到
       **目标释放量**，从最旧起累加 ``encoded_len`` 直到覆盖目标，得到需丢
       条数（不足则全丢）。
    4. 两规则在同一基线上独立计算，取更激进者（丢条数取 max）。
    5. 丢弃顺序恒为最旧优先（消息序 × 消息内序）；返回全部出现的处置，
       kept 与 dropped 按出现序排列。丢弃不改写任何消息——投影方（provider
       序列化）只在被丢弃的出现之后追加固定占位文本块。

    确定性保证：结果只依赖入参（消息列表、policy、vision、cap），同输入
    重复调用结果全等；无时间戳、无随机、无持久状态、不读图片字节。
    """
    plans: list[MediaPlan] = []
    for msg_index, msg in enumerate(messages):
        if msg.role == "system":
            # 见 docstring：system 消息的 media 一律忽略（两协议共同语义）。
            continue
        for ref in msg.media or []:
            plans.append(MediaPlan(message_index=msg_index, ref=ref))

    reasons: list[DropReason | None] = [None] * len(plans)

    if not vision:
        reasons = [DropReason.NO_VISION] * len(plans)
    else:
        if max_image_bytes is not None:
            for i, plan in enumerate(plans):
                if plan.ref.bytes > max_image_bytes:
                    reasons[i] = DropReason.TOO_LARGE
        eligible = [i for i, reason in enumerate(reasons) if reason is None]
        if eligible:
            drop_count = max(
                _count_rule_drops(eligible, policy),
                _byte_rule_drops(eligible, plans, policy),
            )
            for i in eligible[:drop_count]:
                reasons[i] = DropReason.BUDGET

    return [
        MediaPlan(
            message_index=plan.message_index,
            ref=plan.ref,
            drop_reason=reason,
        )
        for plan, reason in zip(plans, reasons, strict=True)
    ]


def _count_rule_drops(eligible: list[int], policy: MediaPolicy) -> int:
    """计数规则：超出 max_images 的量按 count_quantum 向上取整。"""
    excess = len(eligible) - policy.max_images
    if excess <= 0:
        return 0
    return _ceil_div(excess, _quantum(policy.count_quantum)) * _quantum(
        policy.count_quantum
    )


def _byte_rule_drops(
    eligible: list[int], plans: list[MediaPlan], policy: MediaPolicy
) -> int:
    """字节规则：超出预算的量按 evict_quantum_bytes 向上取整，从最旧累加释放。"""
    total = sum(encoded_len(plans[i].ref.bytes) for i in eligible)
    excess = total - policy.request_budget_bytes
    if excess <= 0:
        return 0
    quantum = _quantum(policy.evict_quantum_bytes)
    target = _ceil_div(excess, quantum) * quantum
    freed = 0
    for count, i in enumerate(eligible, start=1):
        freed += encoded_len(plans[i].ref.bytes)
        if freed >= target:
            return count
    return len(eligible)
