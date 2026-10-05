# wing/media.py
"""
wing/media.py — 图片媒体纯函数层（read-image 任务的地基模块）。

职责边界（为什么单独成模块）：
- 只做纯函数与不可变数据结构：字节 → 事实（sha256 id / mime / 尺寸 / 信封
  文本），以及「请求期图片投影」的确定性决策（plan_request_media）；
- 不依赖 config、不依赖 provider、不读存储（投影只需 MediaRef 元数据），
  因此工具层（ReadImage）、provider 序列化层与单测都直接消费本模块。

投影算法（高水位 + 量子批量驱逐）的动机：视觉 token 内联在 token 序列中，
任何让图片表示变化的动作都会**从首个受影响 token 起**打断前缀 cache。
所以策略是「常态一张不丢；只有硬阈值逼到才丢，且按量子批量丢最旧的」，
并保证投影是消息列表的确定性函数（无持久状态、无时间戳）。
"""

from __future__ import annotations

import hashlib
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from enum import Enum

from wing.schema import MediaRef, Message


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


# SOF 标记集合。C4=DHT、C8=JPG、CC=DAC 不是帧头，必须排除。
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
