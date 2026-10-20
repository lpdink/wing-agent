# wing/context/cache_ledger.py
"""会话级 KV 缓存账本：每次 LLM 调用的分段指纹 + provider 报的实际命中 + 归因。

落点 `wing.db` 的两张表（`cache_call` / `cache_segment`，见 `cache_store.py`）：一次调用
一行、它的前缀分段按指纹存一份。旧版本的边车文件 `<session-dir>/cache_ledger.json`
**不再读也不再写**——它留在原地（不删任何文件），新账本从一个空窗口开始。

三条不变量：

- **永不抛**：读、算、写全在 try/except 内，失败只 `log.warning`——记账坏了绝不影响
  LLM 调用（contract §8.2）；
- **有界**：只留本会话最近 `MAX_CALLS` 条调用，段落行随失去引用的指纹回收
  （contract §8.4）；
- **前向容忍**：读不到 / 版本不认识 / 单条记录坏掉一律当空账本（contract §3 末条）——
  库里没有行就是空账本，形状错的行在 SQL 层已被列约束挡住。

归因口径的唯一来源是 contract §4；本模块是它的可执行表达（`record`）与投影
（`build_report`），SQL 全在 `context/cache_store.py`。
"""

from __future__ import annotations

import math
from collections.abc import Callable, Sequence
from dataclasses import replace
from datetime import datetime
from typing import Any

from wing.common.logger import log
from wing.db import Database
from wing.schema import CacheCall, CacheFork, CacheObservation, CachePart, fingerprint
from wing.schema.cache import (
    HISTORY_PREFIX,
    PURPOSE_COMPACT,
    RULES_PREFIX,
    SKILLS_PREFIX,
    SYSTEM_APPEND,
    SYSTEM_BASE,
    SYSTEM_WIRE,
    TOOLS_PREFIX,
    VERDICT_BLOCK_GRANULARITY,
    VERDICT_COLD,
    VERDICT_COLD_EXPIRED,
    VERDICT_HIT,
    VERDICT_PREFIX_BROKEN,
    VERDICT_PROVIDER_UNDERDELIVERED,
)

from .cache_store import CacheStore

MAX_CALLS = 200
"""环形上界（contract §3）：每个会话只留最近 200 条调用。

刻意是模块常量而不是配置项：本功能只观察不干预，不新增配置面。
"""

CACHE_TTL_HOURS = 72
"""超过这个时长的参照调用判 `cold_expired`（contract §4 的 `cache_ttl_hours` 默认值）。

刻意是模块常量而不是配置项：本功能只观察不干预，不新增配置面。
"""

MIN_TOL_TOKENS = 256
TOL_RATIO = 0.02
"""块粒度容差 `tol = max(256, ceil(0.02 * ref.prompt))`（contract §4）。"""

SegmentLookup = Callable[[str | None, int], CachePart | None]
"""按（指纹, 段序）取一段事实的口径——端点投影只要分叉位点那一段。"""


class _PrefixFingerprints:
    """按需计算「本次段列表前 n 段」的指纹（同一个 n 只算一次）。

    `_find_ref` 的判据是「历史调用的段列表是本次的前缀」，等价于「它的指纹等于本次前
    n 段的指纹」（n = 它的段数）。一次性把 N 个前缀指纹全算出来是 O(N²) 字节的哈希
    ——400 段的会话每次记账要烧掉约 1ms，而实际命中的几乎总是最近那条。所以按候选的
    n 惰性算：稳态（前缀延续）只算两次（全量一次 + 最近一条的段数一次，后者通常就是
    前者的补集里最长的那一个）。
    """

    __slots__ = ("_segments", "_cache")

    def __init__(self, segments: Sequence[CachePart], full: str) -> None:
        self._segments = list(segments)
        self._cache: dict[int, str] = {len(self._segments): full}

    @property
    def size(self) -> int:
        """本次段列表的段数（候选比它更长时不可能是前缀）。"""
        return len(self._segments)

    def at(self, length: int) -> str:
        """`fingerprint(segments[:length])`（`length` == 全部时为完整指纹）。"""
        cached = self._cache.get(length)
        if cached is None:
            cached = fingerprint(self._segments[:length])
            self._cache[length] = cached
        return cached


# ── 段落工具 ──────────────────────────────────────────────────────────────


def _pairs(segments: Sequence[CachePart]) -> list[tuple[str, str]]:
    """段落列表的 `(label, sha)` 投影——前缀比较的唯一口径。"""
    return [(segment.label, segment.sha) for segment in segments]


def _common_prefix(left: Sequence[CachePart], right: Sequence[CachePart]) -> int:
    """两个段落列表的最长公共前缀长度。"""
    left_pairs, right_pairs = _pairs(left), _pairs(right)
    limit = min(len(left_pairs), len(right_pairs))
    index = 0
    while index < limit and left_pairs[index] == right_pairs[index]:
        index += 1
    return index


# ── 参照 / 分叉 / 判定（contract §4）──────────────────────────────────────


def _find_ref(
    calls: Sequence[CacheCall], prefixes: _PrefixFingerprints
) -> CacheCall | None:
    """段列表是本次前缀、且有 prompt 的**最近一条**调用（参照调用）。

    倒序扫描（`calls` 已是新→旧）：候选的段列表是本次的前缀 ⟺ 它的指纹等于本次前 n 段
    的指纹（`n` = 它的段数）。段数比本次长的候选直接跳过——连指纹都不必算。

    末尾补一条 twin 回退：段列表与本次**完全相等**的最近调用也算参照（契约只给了严格
    前缀的格子；重试 / 重放这类"没有新增段"的调用否则既无参照也无 fork，判定表落空）。
    完全相等时 `n == size`，`at()` 直接给出全量指纹，与严格前缀走同一条判据。
    """
    for call in calls:
        if call.prompt <= 0:
            continue
        if call.segment_count > prefixes.size:
            continue
        if call.fp == prefixes.at(call.segment_count):
            return call
    return None


def _compute_fork(
    latest: Sequence[CachePart],
    segments: Sequence[CachePart],
    ref_length: int | None,
) -> CacheFork | None:
    """与「最近一条调用」的分叉事实（contract §4 的 `fork`）。

    `index` = 最近一条调用与本次的最长公共前缀长度；仅当 `index < 参照段列表长度`
    （`ref_length`，无参照时取最近一条自身的长度）时才产生分叉——首调用没有历史，
    直接返回 None。`tokens_lost` 由 caller 用参照的字符量补上（要一次聚合查询）。
    """
    if not latest:
        return None
    limit = ref_length if ref_length is not None else len(latest)
    index = _common_prefix(latest, segments)
    if index >= limit:
        return None

    new_label = segments[index].label if index < len(segments) else None
    old_label = latest[index].label if index < len(latest) else None
    new_labels = {segment.label for segment in segments[index:]}
    old_labels = {segment.label for segment in latest[index:]}

    if new_label is None:
        # 本次到尽头（回退 / 压缩后的短链）：参照侧尾部被移除
        kind, label = "remove", old_label
    elif old_label is None:
        kind, label = "insert", new_label
    elif new_label not in old_labels:
        kind, label = "insert", new_label
    elif old_label not in new_labels:
        kind, label = "remove", old_label
    else:
        kind, label = "modify", new_label

    return CacheFork(
        kind=kind,
        index=index,
        label=label or "",
        segments_affected=len(latest) - index,
        tokens_lost=None,
    )


def _tokens_lost(prompt: int, total_chars: int, before_chars: int) -> int:
    """`round(prompt * (1 - chars_before / total_chars))`（估算，展示带 `≈`）。"""
    if total_chars <= 0:
        return 0
    return round(prompt * (1 - before_chars / total_chars))


def _age_hours(ts_ms: int | None, now: datetime) -> float | None:
    """参照调用的年龄（小时）；时间戳缺失 / 坏掉 → None（不判过期）。"""
    if not isinstance(ts_ms, int):
        return None
    return (now.timestamp() * 1000 - ts_ms) / 3_600_000


def _decide(
    ref: CacheCall | None,
    fork: CacheFork | None,
    *,
    cached: int,
    has_history: bool,
    now: datetime,
) -> tuple[str, int | None]:
    """verdict + 理论命中（contract §4 的判定表与优先级）。

    优先级：`cold_expired` > `prefix_broken` > 其余。有 fork（前缀没能延续）一律
    `prefix_broken` 且 `expected = null`。
    """
    if ref is not None:
        prompt = ref.prompt
        age = _age_hours(ref.ts_ms, now)
        if age is not None and age > CACHE_TTL_HOURS:
            return VERDICT_COLD_EXPIRED, None
        if fork is not None:
            return VERDICT_PREFIX_BROKEN, None
        tol = max(MIN_TOL_TOKENS, math.ceil(TOL_RATIO * prompt))
        if cached >= prompt:
            return VERDICT_HIT, prompt
        if prompt - cached <= tol:
            return VERDICT_BLOCK_GRANULARITY, prompt
        return VERDICT_PROVIDER_UNDERDELIVERED, prompt

    if fork is not None or has_history:
        return VERDICT_PREFIX_BROKEN, None
    return VERDICT_COLD, None


# ── 端点投影（contract §6）────────────────────────────────────────────────


def empty_report(session_id: str) -> dict[str, Any]:
    """空账本的投影（无数据 / 会话不存在）：计数 0 + 三档 rate 全 null。"""
    return build_report(session_id, [], limit=0, segment_at=lambda _fp, _i: None)


def build_report(
    session_id: str,
    calls: Sequence[CacheCall],
    *,
    limit: int,
    segment_at: SegmentLookup,
) -> dict[str, Any]:
    """账本 → `GET /api/session/cache` 的响应体。

    `calls` 是**整本账本**（新→旧，调用方保证已按 id 倒序），只有前 `limit` 条进响应体；
    汇总覆盖整本账本，分叉归因还要看窗口外的邻座与参照，因此不接受「只给前 limit 条」。
    """
    calls_out = [
        {
            "ts": call.ts,
            "model": call.model,
            "purpose": call.purpose,
            "prompt": call.prompt,
            "cached": call.cached,
            "expected": call.expected,
            "verdict": call.verdict,
            "fp": call.fp,
        }
        for call in calls[:limit]
    ]

    forks_out: list[dict[str, Any]] = []
    for position, call in enumerate(calls):
        if len(forks_out) >= limit:
            break
        fork = call.fork
        if fork is None:
            continue
        new_segment = segment_at(call.fp, fork.index)
        sha_after = new_segment.sha if new_segment is not None else None
        # 分叉的「另一侧」= 「改前」那一版：记录时分叉相对的那条调用（见 `_baseline`）。
        baseline, ref_ts = _baseline(
            calls,
            position,
            call,
            index=fork.index,
            sha_after=sha_after,
            segment_at=segment_at,
        )
        source = (
            new_segment.src
            if new_segment is not None and new_segment.src is not None
            else (baseline.src if baseline is not None else None)
        )
        blame, avoidable = _blame(call, fork, calls, position)
        forks_out.append(
            {
                "ts": call.ts,
                "index": fork.index,
                "label": fork.label,
                "kind": fork.kind,
                "blame": blame,
                "avoidable": avoidable,
                "tokens_lost": fork.tokens_lost,
                "segments_affected": fork.segments_affected,
                "verdict": call.verdict,
                "detail": {
                    "path": source.path if source is not None else None,
                    "mtime": source.mtime if source is not None else None,
                    "size": source.size if source is not None else None,
                    "ref_ts": ref_ts,
                    "sha_before": baseline.sha if baseline is not None else None,
                    "sha_after": sha_after,
                },
            }
        )

    return {
        "session_id": session_id,
        "summary": _summary(calls),
        "forks": forks_out,
        "calls": calls_out,
    }


def _baseline(
    calls: Sequence[CacheCall],
    position: int,
    call: CacheCall,
    *,
    index: int,
    sha_after: str | None,
    segment_at: SegmentLookup,
) -> tuple[CachePart | None, str | None]:
    """分叉的对照侧（该位点的那一段 + 它的时刻）——「改前」那一版是谁、什么时候。

    参照调用接受的是本次段列表的**前缀**（或完全相同），因此它在自身长度范围内与本次
    逐段同值：`index < len(ref)` 时它的 sha 恒等于 `sha_after`（拿它当 `sha_before` 只会
    重复 `sha_after`，解释不了「这里变了」），`index >= len(ref)` 时它在该位点上没有段。
    能解释这一行的是记录时分叉真正相对的那条调用（`calls[position + 1]`）；参照只在邻座
    已经滚出账本、或邻座在该位点没有段时兜底。

    时刻与被选中的**那一侧**同源：`sha_before` 与 `ref_ts` 永远指向同一条调用，否则
    `--why` 的 sha 行与 ref 行会各说各话。两侧都定位不到（对照侧已经滚出环形账本）时给
    `(None, None)`——不拿邻居的 ts 冒充。
    """
    neighbour = calls[position + 1] if position + 1 < len(calls) else None
    latest = segment_at(neighbour.fp, index) if neighbour is not None else None
    latest_ts = neighbour.ts if neighbour is not None else None
    reference = segment_at(call.ref_fp, index)

    def reference_ts() -> str | None:
        """参照自己的时刻；`ref_fp` 那条调用已经滚出账本 → `None`。"""
        if not call.ref_fp:
            return None
        for older in calls[position + 1 :]:
            if older.fp == call.ref_fp:
                return older.ts
        return None

    if _explains(latest, sha_after):
        return latest, latest_ts
    if _explains(reference, sha_after):
        return reference, reference_ts()
    if neighbour is not None:
        return latest, latest_ts
    return reference, reference_ts()


def _explains(segment: CachePart | None, sha_after: str | None) -> bool:
    """这一侧在该位点上有段、且与本次不同（能当「改前」的证据）。"""
    return segment is not None and segment.sha != sha_after


def _blame(
    call: CacheCall,
    fork: CacheFork,
    calls: Sequence[CacheCall],
    position: int,
) -> tuple[str | None, bool | None]:
    """派生归因（contract §4 的 blame 表；不落盘，读取侧现算）。"""
    verdict = call.verdict
    if verdict == VERDICT_PROVIDER_UNDERDELIVERED:
        return "provider", None
    if verdict in (VERDICT_COLD, VERDICT_COLD_EXPIRED):
        return "cold", None

    label = fork.label
    if label.startswith((RULES_PREFIX, SKILLS_PREFIX)):
        return "external", True
    if label.startswith(TOOLS_PREFIX):
        return "tools", True
    if label.startswith((SYSTEM_BASE, SYSTEM_APPEND, SYSTEM_WIRE)):
        return "composition", False
    if label.startswith(HISTORY_PREFIX):
        if _compact_between(calls, position, call.ref_fp):
            return "compact", False
        return "composition", False
    return "composition", False


def _compact_between(
    calls: Sequence[CacheCall], position: int, ref_fp: str | None
) -> bool:
    """参照调用与本次调用之间是否有过压缩调用（contract §4 的 compact 归因判据）。"""
    if not ref_fp:
        return False
    ref_position = None
    for index in range(position + 1, len(calls)):
        if calls[index].fp == ref_fp:
            ref_position = index
            break
    if ref_position is None:
        return False
    return any(
        calls[index].purpose == PURPOSE_COMPACT
        for index in range(position + 1, ref_position)
    )


def _int(value: int | None) -> int:
    return value if isinstance(value, int) and value > 0 else 0


def _summary(calls: Sequence[CacheCall]) -> dict[str, Any]:
    """汇总（contract §6）：三个 rate 的分母都是 Σ prompt；分母为 0 时全 None。"""
    prompt_total = sum(_int(call.prompt) for call in calls)
    cached_total = sum(_int(call.cached) for call in calls)
    expected_total = sum(_int(call.expected) for call in calls)
    debt_total = sum(
        max(0, call.expected - _int(call.cached))
        for call in calls
        if call.expected is not None
        and call.verdict in (VERDICT_PROVIDER_UNDERDELIVERED, VERDICT_BLOCK_GRANULARITY)
    )
    if prompt_total <= 0:
        return {
            "calls": len(calls),
            "prompt_tokens": prompt_total,
            "cached_tokens": cached_total,
            "expected_tokens": expected_total,
            "theory_rate": None,
            "actual_rate": None,
            "provider_debt_rate": None,
        }
    return {
        "calls": len(calls),
        "prompt_tokens": prompt_total,
        "cached_tokens": cached_total,
        "expected_tokens": expected_total,
        "theory_rate": round(expected_total / prompt_total, 6),
        "actual_rate": round(cached_total / prompt_total, 6),
        "provider_debt_rate": round(debt_total / prompt_total, 6),
    }


# ── 读写口 ────────────────────────────────────────────────────────────────


class CacheLedger:
    """一个会话的账本读写口（进程级 `wing.db`；`session_id` 是存储里的真名）。

    无内存态：每次记录都现读库。`switch_template` 会重建 `ContextManager`，缓存内存态
    会留下两个残影；而一次记录只碰几十行索引与一份段列表，代价远小于一次模型调用。
    """

    def __init__(self, session_id: str, database: Database | None = None) -> None:
        self._session_id = session_id
        self._store = CacheStore(session_id, database)

    @property
    def session_id(self) -> str:
        return self._session_id

    def record(
        self,
        segments: Sequence[CachePart],
        *,
        purpose: str,
        model: str,
        provider: str,
        prompt: int,
        cached: int,
        now: datetime | None = None,
    ) -> CacheObservation | None:
        """记一次调用（best-effort：任何失败只 WARN，返回 None）。"""
        if not segments:
            return None
        try:
            with self._store.transaction() as txn:
                if not txn.live:
                    return None
                observation = self._record(
                    segments,
                    purpose=purpose,
                    model=model,
                    provider=provider,
                    prompt=prompt,
                    cached=cached,
                    now=now,
                )
            if observation is None:
                return None
            if not txn.committed:
                # 语句都返回成功、事务却回滚了（后面的语句失败 / COMMIT 失败）：
                # 账本里没有这条记录，就别回一个「有」的归因（contract §5）。
                log.warning(
                    f"cache ledger record rolled back (session={self._session_id})"
                )
                return None
            return observation
        except Exception as e:
            log.warning(f"cache ledger record failed (session={self._session_id}): {e}")
            return None

    def report(
        self, limit: int = 20, *, session_id: str | None = None
    ) -> dict[str, Any]:
        """端点投影（永不抛）。`session_id` 只用于回显调用方请求的 id。"""
        try:
            calls = self._store.recent(MAX_CALLS)
        except Exception as e:
            log.warning(f"cache ledger unreadable (session={self._session_id}): {e}")
            calls = []
        return build_report(
            session_id or self._session_id,
            calls,
            limit=limit,
            segment_at=self._store.segment_at,
        )

    # ── 一次记录的全过程（已在事务里）───────────────

    def _record(
        self,
        segments: Sequence[CachePart],
        *,
        purpose: str,
        model: str,
        provider: str,
        prompt: int,
        cached: int,
        now: datetime | None,
    ) -> CacheObservation | None:
        stamp = now or datetime.now()
        wire = list(segments)
        fp = fingerprint(wire)
        prefixes = _PrefixFingerprints(wire, fp)

        calls = self._store.recent(MAX_CALLS)
        latest = calls[0] if calls else None
        ref = _find_ref(calls, prefixes)
        fork = self._fork(wire, prefixes, ref, latest)
        verdict, expected = _decide(
            ref, fork, cached=cached, has_history=latest is not None, now=stamp
        )
        if fork is not None and ref is not None:
            total, before = self._store.char_sums(ref.fp, fork.index)
            fork = replace(fork, tokens_lost=_tokens_lost(ref.prompt, total, before))

        # 三笔写要么一起落库、要么一起回滚（同一个事务）：没落库就别回一个「账本里
        # 有」的归因——直播字段与账本必须同源（contract §5）。
        landed = (
            self._store.insert_call(
                CacheCall(
                    ts=stamp.isoformat(timespec="milliseconds"),
                    fp=fp,
                    purpose=purpose,
                    model=model,
                    provider=provider,
                    prompt=prompt,
                    cached=cached,
                    verdict=verdict,
                    segment_count=len(wire),
                    ts_ms=int(stamp.timestamp() * 1000),
                    expected=expected,
                    ref_fp=ref.fp if ref is not None else None,
                    fork=fork,
                )
            )
            and self._store.save_segments(fp, wire)
            and self._store.trim(MAX_CALLS)
        )
        if not landed:
            log.warning(
                f"cache ledger record lost (session={self._session_id}, fp={fp})"
            )
            return None
        return CacheObservation(
            expected=expected,
            verdict=verdict,
            diverged=fork.label if fork is not None else "",
        )

    def _fork(
        self,
        segments: Sequence[CachePart],
        prefixes: _PrefixFingerprints,
        ref: CacheCall | None,
        latest: CacheCall | None,
    ) -> CacheFork | None:
        """与最近一条调用的分叉事实（前缀延续时一次段列表都不读）。

        「最新一条调用的段列表就是本次的前缀」⟺ 它的指纹等于本次前 n 段的指纹
        （n = 它的段数）——一次哈希（惰性、多数情况早已算过）就能定案，不必把它的
        段落表读出来逐段比对。前缀破碎才是慢路径（读那一份段列表）。
        """
        if latest is None:
            return None
        ref_length = ref.segment_count if ref is not None else None
        if latest.fp == prefixes.at(latest.segment_count) and (
            ref_length is None or latest.segment_count >= ref_length
        ):
            # 前缀延续（或是同一条请求的重放）：没有分叉
            return None
        return _compute_fork(self._store.segments(latest.fp), segments, ref_length)
