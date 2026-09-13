# wing/gateway/frames.py — wire 帧切分（软上限切分 / 硬上限契约）

"""Wire 帧切分——保证任何出网帧都不超过客户端单帧上限。

背景：客户端（tokio-tungstenite）单帧上限是 **16 MiB** 默认值，超过即读任务
死亡（`Message too long: 22151988 > 16777216`）。PR #68 之后 `sync_session`
载荷可以远超它（2026-09-13 实测 22,151,988 B），因此上界必须由**发送方**保证：
超过软上限（8 MiB）的载荷在唯一 wire 出口按 UTF-8 边界切分为 N 帧，每帧携带
最小信封（`type == "_chunk"`）；接收方在低层合并还原，应用层零感知。

本模块是纯逻辑（无 I/O、不依赖事件类型），便于边界单测：

- `build_frames(payload, of_type) -> list[Frame]`：载荷 → 帧列表。
- `Frame.size` 是**精确**的 UTF-8 字节数（构造期算好，发送侧不再重复 encode）。

不变量（由本模块保证，spec `ws-frame-chunking`）：

1. 每帧 ≤ `SOFT_LIMIT_BYTES`（含信封与 JSON 转义开销）；
2. 所有帧 `data` 顺序拼接后与原始载荷逐字节相等；
3. 切点落在 UTF-8 字符边界；
4. 每帧尽量接近软上限（帧数最小化）；
5. `HARD_LIMIT_BYTES` 是发送侧的最终契约（安全网，正常路径永不触发）。
"""

from __future__ import annotations

import itertools
import json
import re
from collections.abc import Callable
from dataclasses import dataclass

# 软上限：载荷超过它即切分（切分后每帧 ≤ 它）。硬编码——与客户端上限的
# 比例关系是契约（留一倍余量吸收信封与转义开销），不是可调策略。
SOFT_LIMIT_BYTES = 8 * 1024 * 1024

# 硬上限：任何出网帧不得超过它。数值 MUST 等于客户端 tungstenite 默认
# `max_frame_size`（16 MiB）——超过它客户端读任务会直接死亡。
HARD_LIMIT_BYTES = 16 * 1024 * 1024

# 分片信封的 `type`。以 `_` 开头为传输层保留命名（应用事件不得使用）。
CHUNK_EVENT_TYPE = "_chunk"

# 控制字符检测：`json.dumps` 的输出里它们必然是转义形态，因此常态下走快路径
# 的转义计数（`"` / `\`）。真出现裸控制字符时退回逐片精确测量（慢但正确）。
_CONTROL_RE = re.compile(rb"[\x00-\x1f]")

_chunk_ids = itertools.count(1)


@dataclass(frozen=True)
class Frame:
    """一个待发送的文本帧 + 精确 UTF-8 字节数。"""

    text: str
    size: int


def _align_back(raw: bytes, end: int, start: int) -> int:
    """把字节下标 `end` 回退到 UTF-8 字符边界（不越过 `start`）。"""
    while start < end < len(raw) and (raw[end] & 0xC0) == 0x80:
        end -= 1
    return end


def _next_boundary(raw: bytes, start: int) -> int:
    """`start` 之后最近的下一个 UTF-8 字符边界（至少前进一个完整字符）。"""
    end = start + 1
    while end < len(raw) and (raw[end] & 0xC0) == 0x80:
        end += 1
    return end


def _fast_measure(raw: bytes) -> Callable[[int, int], int]:
    """返回 `measure(start, end) -> 转义后字节数`。

    快路径：`json.dumps(piece, ensure_ascii=False)` 只把 `"` 与 `\\` 变长
    （各 +1 字节），控制字符在载荷里已经是转义形态（载荷是 `json.dumps` 的
    输出）——因此 span + 两类字符计数即精确值。载荷含裸控制字符时退回逐片
    `json.dumps(...).encode()` 精确测量。
    """
    if _CONTROL_RE.search(raw) is None:

        def measure(start: int, end: int) -> int:
            return (
                (end - start)
                + raw.count(b'"', start, end)
                + raw.count(b"\\", start, end)
            )

        return measure

    def exact_measure(start: int, end: int) -> int:
        piece = raw[start:end].decode("utf-8")
        return len(json.dumps(piece, ensure_ascii=False).encode("utf-8"))

    return exact_measure


def _split_raw(
    raw: bytes, budget: int, measure: Callable[[int, int], int]
) -> list[bytes]:
    """按「转义后 ≤ budget」切分 `raw`（每片都是合法 UTF-8 片段）。

    每片都尽量吃满预算：先按字节预算取候选，转义膨胀时按「当前片的转义密度」
    等比收缩（`span * budget / used`——对 `"` / `\\` 这类等密度膨胀一步收敛）。
    """
    pieces: list[bytes] = []
    start = 0
    total = len(raw)
    while start < total:
        end = _align_back(raw, min(start + budget, total), start)
        while end > start:
            used = measure(start, end)
            if used <= budget:
                break
            span = end - start
            end = _align_back(raw, start + max(1, span * budget // used), start)
            if end <= start:
                end = _next_boundary(raw, start)
                break
        if end <= start:  # 预算小到装不下一个字符——前进一个完整字符（不产空片）
            end = _next_boundary(raw, start)
        pieces.append(raw[start:end])
        start = end
    return pieces


# 帧的固定收尾开销：`data` 值两侧的引号 + 信封的收尾 `}`（都是 ASCII）。
_FRAME_TAIL = 3


def _head(chunk_id: str, of_type: str, count: int, index: int) -> str:
    """信封除 `data` 值本身以外的部分（纯 ASCII——`len()` 即字节数）。

    注意 `data` 值两侧的引号与收尾 `}` 不在其中，它们计入 `_FRAME_TAIL`。
    """
    return (
        f'{{"type":"{CHUNK_EVENT_TYPE}"'
        f',"id":{json.dumps(chunk_id)}'
        f',"index":{index}'
        f',"count":{count}'
        f',"of_type":{json.dumps(of_type)}'
        f',"data":'
    )


def _frame(chunk_id: str, of_type: str, count: int, index: int, piece: bytes) -> Frame:
    """把一段原始载荷包成信封帧（`size` 精确到字节）。"""
    dumped = json.dumps(piece.decode("utf-8"), ensure_ascii=False)
    head = _head(chunk_id, of_type, count, index)
    return Frame(
        text=head + dumped + "}",
        size=len(head) + len(dumped.encode("utf-8")) + 1,
    )


def build_frames(payload: str, of_type: str) -> list[Frame]:
    """把事件载荷切成「每帧 ≤ 软上限」的帧列表。

    小载荷（绝大多数事件）原样返回单帧——快路径不 encode、不切分，零额外开销。
    """
    if len(payload) <= SOFT_LIMIT_BYTES and payload.isascii():
        return [Frame(text=payload, size=len(payload))]

    raw = payload.encode("utf-8")
    if len(raw) <= SOFT_LIMIT_BYTES:
        return [Frame(text=payload, size=len(raw))]

    chunk_id = str(next(_chunk_ids))
    measure = _fast_measure(raw)
    # 帧数的保守估计：只用于信封数字位数的预留（真实帧数只会更少）。
    count_hint = len(raw) // SOFT_LIMIT_BYTES + 2

    def overhead(count: int) -> int:
        """最坏情况下（`index` 位数最多）一帧除 `data` 转义内容外的字节数。"""
        return (
            max(len(_head(chunk_id, of_type, count, index)) for index in range(count))
            + _FRAME_TAIL
        )

    reserve = overhead(count_hint)
    frames: list[Frame] = []
    for _ in range(4):
        pieces = _split_raw(raw, max(SOFT_LIMIT_BYTES - reserve, 2), measure)
        count = len(pieces)
        frames = [
            _frame(chunk_id, of_type, count, index, piece)
            for index, piece in enumerate(pieces)
        ]
        # 用真实帧数重算信封预留：预算成立即返回（首轮必然成立——count ≤ hint）。
        worst = overhead(count)
        if worst <= reserve and all(f.size <= SOFT_LIMIT_BYTES for f in frames):
            return frames
        reserve = worst
    return frames  # pragma: no cover — 收敛在 1~2 轮内，兜底分支不可达
