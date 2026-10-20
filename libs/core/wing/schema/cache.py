# wing/schema/cache.py
"""缓存账本的领域类型与纯函数（契约 §1–§5 的词汇表）。

一次 LLM 请求的前缀被切成**有序具名段**；本模块只负责「段落 → 事实」的纯计算：段 label 的
唯一声明处、确定性规范形、段落 hash、指纹，以及 provider 侧探针（`CacheProbe`）与归因结果
（`CacheObservation`）的载体。这里不持有状态、不触碰 I/O——账本读写归
`wing.context.cache_ledger`。

**探针只观察不改写**：`CacheProbe` 由 caller 每次调用新建并挂到
`RequestOptions.cache_probe`；`None` 表示探针关闭（provider 一个分段都不算）。
"""

from __future__ import annotations

import hashlib
import json
from dataclasses import dataclass
from typing import Any, Sequence

# ── 段 label（契约 §1 的唯一声明处）────────────────────────────────────────

SYSTEM_BASE = "system:base"
""""模板自带的系统提示词主体（ContextManager 产出）。"""

SYSTEM_APPEND = "system:append"
"""会话级附加系统提示（`append_system_prompt`；ContextManager 产出）。"""

SYSTEM_WIRE = "system:wire"
"""封口段：实际下发的 system 载荷（协议序列化后，provider 产出）。"""

RULES_PREFIX = "rules:"
SKILLS_PREFIX = "skills:"
TOOLS_PREFIX = "tools:"
HISTORY_PREFIX = "history:"

# ── 归因词汇（契约 §4 / §5 的唯一声明处）─────────────────────────────────

VERDICT_HIT = "hit"
VERDICT_BLOCK_GRANULARITY = "block_granularity"
VERDICT_PROVIDER_UNDERDELIVERED = "provider_underdelivered"
VERDICT_PREFIX_BROKEN = "prefix_broken"
VERDICT_COLD = "cold"
VERDICT_COLD_EXPIRED = "cold_expired"

PURPOSE_MAIN = "main"
PURPOSE_COMPACT = "compact"


def rules_label(path: str) -> str:
    return f"{RULES_PREFIX}{path}"


def skills_label(name: str) -> str:
    return f"{SKILLS_PREFIX}{name}"


def tools_label(name: str) -> str:
    return f"{TOOLS_PREFIX}{name}"


def history_label(uuid: str | None, role: str) -> str:
    """`history:<uuid>:<role>`；uuid 缺失（如压缩请求临时拼的 prompt）记作 `-`。"""
    return f"{HISTORY_PREFIX}{uuid or '-'}:{role}"


# ── 确定性规范形与 hash（契约 §1 / §2）────────────────────────────────────


def canonical_json(obj: Any) -> str:
    """确定性规范形：排序键 + 不转义非 ASCII + 无分隔空白。

    hash 的输入一律过这里——文本段传字符串（规范形即 JSON 字符串字面量），协议段传序列化
    后的 dict，两种输入共用同一条规则，不会因「文本 / dict 两套口径」而漂移。
    """
    return json.dumps(obj, sort_keys=True, ensure_ascii=False, separators=(",", ":"))


def _sha_of(canonical: str) -> str:
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()[:16]


def sha16(obj: Any) -> str:
    """规范形的 sha256 前 16 位（契约 §1 的 `sha`）。"""
    return _sha_of(canonical_json(obj))


def fingerprint(segments: Sequence["CachePart"]) -> str:
    """指纹（契约 §2）：`sha256("|".join(f"{label}:{sha}"))[:16]`。

    段列表相同 ⇒ 指纹相同。用途只有两处：段落表按它去重，以及「这条历史调用的段列表
    是不是本次的前缀」的判定（它的指纹 == 本次前 n 段的指纹）。不参与任何算术。
    """
    return _sha_of("|".join(f"{segment.label}:{segment.sha}" for segment in segments))


# ── 段落 ──────────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class CacheSource:
    """段的来源文件事实（contract §1 的 `src`）。

    只在 `rules:*` / `skills:*` 段上携带，**加载时采集一次**（热路径不扫目录、不 stat）。
    `mtime` 是本地 naive ISO（与 `metadata.json` 同一时间口径）。
    """

    path: str
    mtime: str | None = None
    size: int | None = None

    def to_wire(self) -> dict[str, Any]:
        return {"path": self.path, "mtime": self.mtime, "size": self.size}


@dataclass(frozen=True)
class CachePart:
    """一段前缀事实：`{label, sha, chars, src?}`。

    `chars` 是该段文本的字符数，**只用于估算代价**（`tokens_lost`），不参与判定。
    """

    label: str
    sha: str
    chars: int
    src: CacheSource | None = None

    def to_wire(self) -> dict[str, Any]:
        wire: dict[str, Any] = {
            "label": self.label,
            "sha": self.sha,
            "chars": self.chars,
        }
        if self.src is not None:
            wire["src"] = self.src.to_wire()
        return wire

    @classmethod
    def text(cls, label: str, text: str, src: CacheSource | None = None) -> "CachePart":
        """文本段（system 块 / rules 文件 / skill 片段）：`chars` = 文本长度。"""
        return cls(
            label=label, sha=_sha_of(canonical_json(text)), chars=len(text), src=src
        )

    @classmethod
    def obj(cls, label: str, obj: Any, src: CacheSource | None = None) -> "CachePart":
        """协议序列化段（tools / history / system:wire）：hash 与 chars 都取规范形。"""
        canonical = canonical_json(obj)
        return cls(label=label, sha=_sha_of(canonical), chars=len(canonical), src=src)


# ── 分叉与归因结果 ────────────────────────────────────────────────────────


@dataclass(frozen=True)
class CacheFork:
    """前缀分叉事实（契约 §4 的 `fork`）；`tokens_lost` 是**估算值**。"""

    kind: str
    index: int
    label: str
    segments_affected: int
    tokens_lost: int | None = None

    def to_wire(self) -> dict[str, Any]:
        return {
            "kind": self.kind,
            "index": self.index,
            "label": self.label,
            "segments_affected": self.segments_affected,
            "tokens_lost": self.tokens_lost,
        }


@dataclass(frozen=True)
class CacheCall:
    """账本里的一次调用（`cache_call` 一行的内存形态）。

    `ts` 是本地 naive ISO（对外展示与 `forks[].detail.ref_ts` 都用它）；`ts_ms` 只用于
    判「参照调用是否过期」，缺失 / 不可解析时为 None。`segment_count` 是这次调用前缀的
    段数（前缀比较的边界）；`expected is None` = 这一轮没有理论水位（cold / 分叉 /
    过期）；`fork` 非空 = 前缀没能延续（契约 §4）。
    """

    ts: str
    fp: str
    purpose: str
    model: str
    provider: str
    prompt: int
    cached: int
    verdict: str
    segment_count: int = 0
    ts_ms: int | None = None
    expected: int | None = None
    ref_fp: str | None = None
    fork: CacheFork | None = None


@dataclass(frozen=True)
class CacheObservation:
    """一次调用的归因结果（`llm_call_metrics` 的三个直播字段直接取自这里）。"""

    expected: int | None = None
    verdict: str = ""
    diverged: str = ""


class CacheProbe:
    """一次 LLM 调用的分段累积器（caller 创建 → provider 写入 → caller 消费）。

    provider 在**构建 body 的同一处**写下 `[*CM 侧段落, *wire 侧段落]`（覆盖式，不追加——
    重试重放同一请求时不会把段落翻倍）。探针不持有账本、不写盘：记账是 caller 的事。
    """

    __slots__ = ("segments",)

    def __init__(self) -> None:
        self.segments: list[CachePart] = []

    def set_segments(self, segments: Sequence[CachePart]) -> None:
        self.segments = list(segments)
