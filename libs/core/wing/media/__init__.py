# wing/media/__init__.py
"""wing/media 包 — 图片媒体纯函数层（read-image 任务的地基模块）。

职责边界（为什么单独成包）：
- 只做纯函数与不可变数据结构：字节 → 事实（``facts``：sha256 id / mime / 尺寸 /
  信封文本），以及「请求期图片投影」的确定性决策（``policy``：高水位 + 量子驱逐）；
- 不依赖 config、不依赖 provider、不读存储（投影只需 MediaRef 元数据），
  因此工具层（ReadImage）、provider 序列化层与单测都直接消费本包。

公共 API 通过此 __init__ re-export（消费方 import 路径不变）：

    from wing.media import plan_request_media, MediaPolicy, media_id, ...

（MediaRef / Message 的本体在 wing.schema；本包为兼容旧 `wing/media.py` 的命名空间
一并 re-export——新代码应继续从 wing.schema 导入。）
"""

from wing.schema import MediaRef, Message

from .facts import (
    SUPPORTED_IMAGE_MIMES,
    encoded_len,
    estimate_image_tokens,
    format_image_envelope,
    format_size,
    image_dimensions,
    is_apple_cgbi_png,
    media_id,
    sniff_image_mime,
)
from .policy import (
    BUDGET_PLACEHOLDER,
    NO_VISION_PLACEHOLDER,
    PLACEHOLDER_BY_REASON,
    TOO_LARGE_PLACEHOLDER,
    DropReason,
    MediaAccess,
    MediaPlan,
    MediaPolicy,
    plan_request_media,
)

__all__ = [
    "BUDGET_PLACEHOLDER",
    "DropReason",
    "MediaAccess",
    "MediaPlan",
    "MediaPolicy",
    "MediaRef",
    "Message",
    "NO_VISION_PLACEHOLDER",
    "PLACEHOLDER_BY_REASON",
    "SUPPORTED_IMAGE_MIMES",
    "TOO_LARGE_PLACEHOLDER",
    "encoded_len",
    "estimate_image_tokens",
    "format_image_envelope",
    "format_size",
    "image_dimensions",
    "is_apple_cgbi_png",
    "media_id",
    "plan_request_media",
    "sniff_image_mime",
]
