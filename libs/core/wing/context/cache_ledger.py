# wing/context/cache_ledger.py
"""会话级 KV 缓存账本：每次 LLM 调用的分段指纹 + provider 报的实际命中 + 归因。

落点 `<session-dir>/cache_ledger.json`——经 `MessageLog` 的 aux kv 通道写入（file 后端原子写
= tmp + fsync + rename，memory 后端留在进程内字典）。**不新增存储接口**，也绝不碰
`history.jsonl`。

三条不变量：

- **永不抛**：读、算、写全在 try/except 内，失败只 `log.warning`——记账坏了绝不影响 LLM 调用
  （contract §8.2）；
- **有界**：`calls` 只留最近 `MAX_CALLS` 条，`fingerprints` 只留被这些 calls 引用到的条目
  （写盘时 GC，contract §8.4）；
- **前向容忍**：读不到 / 版本不认识 / 解析失败一律当空账本（contract §3 末条）。

归因口径的唯一来源是 contract §4；本模块只是它的可执行表达（`record_call`）。
"""

from __future__ import annotations

import json
import math
import threading
from datetime import datetime
from typing import Any, Sequence

from wing.common.logger import log
from wing.schema import CacheFork, CacheObservation, CachePart, fingerprint
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

CACHE_LEDGER_KEY = "cache_ledger"
"""aux kv 的 key：file 后端因此落 `<session-dir>/cache_ledger.json`。"""

LEDGER_VERSION = 1

MAX_CALLS = 200
"""环形上界（contract §3）：`calls` 只留最近 200 条。"""

CACHE_TTL_HOURS = 72
"""超过这个时长的参照调用判 `cold_expired`（contract §4 的 `cache_ttl_hours` 默认值）。

刻意是模块常量而不是配置项：本功能只观察不干预，不新增配置面。
"""

MIN_TOL_TOKENS = 256
TOL_RATIO = 0.02
"""块粒度容差 `tol = max(256, ceil(0.02 * ref.prompt))`（contract §4）。"""

MAX_LEDGER_BYTES = 512 * 1024
"""账本落盘的字节预算（`json.dumps(..., ensure_ascii=False)` 的 UTF-8 长度，与
`MessageLog.write_aux` 同一序列化口径）。

段落表按调用冗余存储：常规 40 轮会话约 160 KB，200 条 × 上百段的极端形态能到数 MB
（contract §3 的「~60KB/会话」按「多条调用共享同一份段落表」估，见 docs/dev）。超出预算
就从**最旧调用**开始淘汰（连带 GC 失去引用的指纹），永远保留最近一条调用。
"""


# ── 账本形状 ──────────────────────────────────────────────────────────────


def empty_ledger(session_id: str) -> dict[str, Any]:
    """空账本（version 1）。读不到 / 解析失败 / 首调用前的唯一形态。"""
    return {
        "version": LEDGER_VERSION,
        "session_id": session_id,
        "fingerprints": {},
        "calls": [],
    }


def normalize_ledger(raw: Any, session_id: str) -> dict[str, Any]:
    """把盘上的原始数据规整成账本（永不抛；不认识的一律降级为空账本）。

    逐字段归一是**边界契约**：账本是可被手改 / 被新版本写过的边车文件，而端点
    用强类型响应模型序列化它——一条类型错位的记录不该让查询 500（契约 §3「读不到 /
    解析失败 ⇒ 当作空账本」的精神：单条坏记录丢弃，不升级成整份失败）。
    """
    if not isinstance(raw, dict) or raw.get("version") != LEDGER_VERSION:
        return empty_ledger(session_id)
    fingerprints_raw = raw.get("fingerprints")
    calls_raw = raw.get("calls")
    calls = (
        [
            call
            for call in (_normalize_call(entry) for entry in calls_raw)
            if call is not None
        ]
        if isinstance(calls_raw, list)
        else []
    )
    return {
        "version": LEDGER_VERSION,
        "session_id": raw.get("session_id")
        if isinstance(raw.get("session_id"), str)
        else session_id,
        "fingerprints": fingerprints_raw if isinstance(fingerprints_raw, dict) else {},
        "calls": calls,
    }


_CALL_TEXT_FIELDS = ("ts", "fp", "purpose", "model", "provider", "verdict")


def _is_int(value: Any) -> bool:
    """真整数才算计数：`bool` 是 `int` 的子类，手改账本写成 `true` 不能当 1 收下。"""
    return isinstance(value, int) and not isinstance(value, bool)


def _normalize_call(entry: Any) -> dict[str, Any] | None:
    """单条调用记录的类型归一（坏记录返回 None = 丢弃）。"""
    if not isinstance(entry, dict):
        return None
    call: dict[str, Any] = {}
    for field in _CALL_TEXT_FIELDS:
        value = entry.get(field)
        if not isinstance(value, str):
            return None
        call[field] = value
    for field in ("prompt", "cached", "expected"):
        value = entry.get(field)
        call[field] = value if _is_int(value) else None
    if call["prompt"] is None or call["cached"] is None:
        return None
    ref_fp = entry.get("ref_fp")
    call["ref_fp"] = ref_fp if isinstance(ref_fp, str) else None
    fork = entry.get("fork")
    call["fork"] = _normalize_fork(fork) if isinstance(fork, dict) else None
    return call


def _normalize_fork(raw: dict[str, Any]) -> dict[str, Any] | None:
    kind, label = raw.get("kind"), raw.get("label")
    index, affected = raw.get("index"), raw.get("segments_affected")
    lost = raw.get("tokens_lost")
    if not isinstance(kind, str) or not isinstance(label, str):
        return None
    return {
        "kind": kind,
        "index": index if _is_int(index) else 0,
        "label": label,
        "segments_affected": affected if _is_int(affected) else 0,
        "tokens_lost": lost if _is_int(lost) else None,
    }


def _segments_of(fingerprints: dict[str, Any], fp: Any) -> list[dict[str, Any]]:
    """取某个指纹对应的段落列表（缺失 / 形状不对 → 空列表）。"""
    if not isinstance(fp, str):
        return []
    entry = fingerprints.get(fp)
    if not isinstance(entry, dict):
        return []
    segments = entry.get("segments")
    if not isinstance(segments, list):
        return []
    return [segment for segment in segments if isinstance(segment, dict)]


def _pairs(segments: Sequence[dict[str, Any]]) -> list[tuple[Any, Any]]:
    """段落列表的 `(label, sha)` 投影——前缀比较的唯一口径。"""
    return [(segment.get("label"), segment.get("sha")) for segment in segments]


def _common_prefix(left: Sequence[dict], right: Sequence[dict]) -> int:
    """两个段落列表的最长公共前缀长度。"""
    left_pairs, right_pairs = _pairs(left), _pairs(right)
    limit = min(len(left_pairs), len(right_pairs))
    index = 0
    while index < limit and left_pairs[index] == right_pairs[index]:
        index += 1
    return index


# ── 参照 / 分叉 / 判定（contract §4）──────────────────────────────────────


def _find_ref(
    calls: Sequence[dict[str, Any]],
    fingerprints: dict[str, Any],
    segments: Sequence[dict[str, Any]],
) -> tuple[dict[str, Any] | None, list[dict[str, Any]]]:
    """倒序找参照调用：最近的、段列表是本次**严格前缀**且 prompt > 0 的调用。

    末尾补一条 twin 回退：段列表与本次**完全相等**的最近调用也算参照（契约只给了严格
    前缀的格子；重试 / 重放这类"没有新增段"的调用否则既无参照也无 fork，判定表落空
    ——否则「重试 / 重放」这类没有新增段的调用既无参照也无 fork，判定表落空）。
    """
    for index in range(len(calls) - 1, -1, -1):
        call = calls[index]
        prompt = call.get("prompt")
        if not isinstance(prompt, int) or prompt <= 0:
            continue
        candidate = _segments_of(fingerprints, call.get("fp"))
        if not candidate or len(candidate) > len(segments):
            continue
        if _pairs(candidate) != _pairs(segments[: len(candidate)]):
            continue
        return call, candidate
    return None, []


def _compute_fork(
    latest: Sequence[dict[str, Any]],
    segments: Sequence[dict[str, Any]],
    ref: dict[str, Any] | None,
    ref_segments: Sequence[dict[str, Any]],
) -> CacheFork | None:
    """与「最近一条调用」的分叉事实（contract §4 的 `fork`）。

    `k` = 最近一条调用与本次的最长公共前缀长度；仅当 `k < 参照段列表长度` 时才产生分叉
    （无参照时以最近一条自身的长度为准——首调用没有历史，直接返回 None）。
    """
    if not latest:
        return None
    limit = len(ref_segments) if ref is not None else len(latest)
    index = _common_prefix(latest, segments)
    if index >= limit:
        return None

    new_label = segments[index].get("label") if index < len(segments) else None
    old_label = latest[index].get("label") if index < len(latest) else None
    new_labels = {segment.get("label") for segment in segments[index:]}
    old_labels = {segment.get("label") for segment in latest[index:]}

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
        kind=str(kind),
        index=index,
        label=str(label or ""),
        segments_affected=len(latest) - index,
        tokens_lost=_tokens_lost(ref, ref_segments, index),
    )


def _tokens_lost(
    ref: dict[str, Any] | None,
    ref_segments: Sequence[dict[str, Any]],
    index: int,
) -> int | None:
    """`round(ref.prompt * (1 - chars_before / ref_total_chars))`（估算，展示带 `≈`）。"""
    if ref is None:
        return None
    prompt = ref.get("prompt")
    if not isinstance(prompt, int):
        return None
    total = sum(_chars(segment) for segment in ref_segments)
    if total <= 0:
        return 0
    before = sum(_chars(segment) for segment in ref_segments[:index])
    return round(prompt * (1 - before / total))


def _chars(segment: dict[str, Any]) -> int:
    chars = segment.get("chars")
    return chars if isinstance(chars, int) and chars > 0 else 0


def _age_hours(ts: Any, now: datetime) -> float | None:
    """参照调用的年龄（小时）；时间戳缺失 / 不可解析 → None（不判过期）。"""
    if not isinstance(ts, str):
        return None
    try:
        return (now - datetime.fromisoformat(ts)).total_seconds() / 3600
    except ValueError:
        return None


def _decide(
    ref: dict[str, Any] | None,
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
        prompt = ref.get("prompt")
        prompt = prompt if isinstance(prompt, int) else 0
        age = _age_hours(ref.get("ts"), now)
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


# ── 记一次调用 ────────────────────────────────────────────────────────────


def record_call(
    data: dict[str, Any],
    segments: Sequence[CachePart],
    *,
    purpose: str,
    model: str,
    provider: str,
    prompt: int,
    cached: int,
    now: datetime | None = None,
) -> tuple[dict[str, Any], CacheObservation]:
    """把一次调用记进账本，返回（新账本, 归因）。纯函数——I/O 归 `CacheLedger`。"""
    stamp = now or datetime.now()
    ts = stamp.isoformat(timespec="milliseconds")
    wire = [segment.to_wire() for segment in segments]
    fp = fingerprint(segments)

    calls = list(data.get("calls") or [])
    fingerprints = dict(data.get("fingerprints") or {})

    ref, ref_segments = _find_ref(calls, fingerprints, wire)
    latest = _segments_of(fingerprints, calls[-1].get("fp")) if calls else []
    fork = _compute_fork(latest, wire, ref, ref_segments)
    verdict, expected = _decide(
        ref, fork, cached=cached, has_history=bool(calls), now=stamp
    )

    calls.append(
        {
            "ts": ts,
            "fp": fp,
            "purpose": purpose,
            "model": model,
            "provider": provider,
            "prompt": prompt,
            "cached": cached,
            "expected": expected,
            "ref_fp": ref.get("fp") if ref is not None else None,
            "verdict": verdict,
            "fork": fork.to_wire() if fork is not None else None,
        }
    )
    trimmed = calls[-MAX_CALLS:]
    fingerprints[fp] = {"segments": wire}
    # GC：只留被存活的 calls 引用到的指纹（否则 200 条的环会长出孤儿段落表）
    alive = {call.get("fp") for call in trimmed}
    fingerprints = {key: value for key, value in fingerprints.items() if key in alive}

    return (
        _fit_budget({**data, "calls": trimmed, "fingerprints": fingerprints}),
        CacheObservation(
            expected=expected,
            verdict=verdict,
            diverged=fork.label if fork is not None else "",
        ),
    )


def _ledger_bytes(data: dict[str, Any]) -> int:
    return len(json.dumps(data, ensure_ascii=False).encode("utf-8"))


def _fit_budget(data: dict[str, Any]) -> dict[str, Any]:
    """把账本压进 `MAX_LEDGER_BYTES`：最旧调用先淘汰，至少留最近一条。

    批量淘汰（一次丢最旧的 1/8）而不是逐条：大账本上逐条全量序列化是 O(n²)。裁剪后同步
    回收失去引用的指纹——孤儿段落表比 calls 本身更占地方。
    """
    calls = list(data.get("calls") or [])
    fingerprints = dict(data.get("fingerprints") or {})
    if _ledger_bytes(data) <= MAX_LEDGER_BYTES:
        return data
    while len(calls) > 1:
        calls = calls[max(1, len(calls) // 8) :]
        alive = {call.get("fp") for call in calls}
        fingerprints = {
            key: value for key, value in fingerprints.items() if key in alive
        }
        fitted = {**data, "calls": calls, "fingerprints": fingerprints}
        if _ledger_bytes(fitted) <= MAX_LEDGER_BYTES:
            return fitted
    return {**data, "calls": calls, "fingerprints": fingerprints}


# ── 端点投影（contract §6）────────────────────────────────────────────────


def build_report(
    data: dict[str, Any], *, session_id: str, limit: int
) -> dict[str, Any]:
    """账本 → `GET /api/session/cache` 的响应体（新→旧，forks 只含有分叉的调用）。"""
    calls = list(data.get("calls") or [])
    fingerprints = data.get("fingerprints") or {}
    recent = list(reversed(calls))

    calls_out = [
        {
            "ts": call.get("ts") or "",
            "model": call.get("model") or "",
            "purpose": call.get("purpose") or "",
            "prompt": call.get("prompt") if isinstance(call.get("prompt"), int) else 0,
            "cached": call.get("cached") if isinstance(call.get("cached"), int) else 0,
            "expected": call.get("expected"),
            "verdict": call.get("verdict") or "",
            "fp": call.get("fp") or "",
        }
        for call in recent[:limit]
    ]

    forks_out: list[dict[str, Any]] = []
    for position, call in enumerate(recent):
        if len(forks_out) >= limit:
            break
        fork = CacheFork.from_wire(call.get("fork"))
        if fork is None:
            continue
        new_segments = _segments_of(fingerprints, call.get("fp"))
        sha_after = _segment_sha(new_segments, fork.index)
        # 分叉的「另一侧」= 「改前」那一版：记录时分叉相对的那条调用（见 `_baseline`）。
        baseline = _baseline(
            recent,
            position,
            call,
            fingerprints,
            index=fork.index,
            sha_after=sha_after,
        )
        source = _segment_source(new_segments, fork.index) or _segment_source(
            baseline, fork.index
        )
        blame, avoidable = _blame(call, fork, recent, position)
        forks_out.append(
            {
                "ts": call.get("ts") or "",
                "index": fork.index,
                "label": fork.label,
                "kind": fork.kind,
                "blame": blame,
                "avoidable": avoidable,
                "tokens_lost": fork.tokens_lost,
                "segments_affected": fork.segments_affected,
                "verdict": call.get("verdict") or "",
                "detail": {
                    "path": source.get("path") if source else None,
                    "mtime": source.get("mtime") if source else None,
                    "size": source.get("size") if source else None,
                    "ref_ts": _ref_ts(
                        recent, position, call.get("ref_fp"), fallback=position + 1
                    ),
                    "sha_before": _segment_sha(baseline, fork.index),
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


def _segment_source(
    segments: Sequence[dict[str, Any]], index: int
) -> dict[str, Any] | None:
    if index >= len(segments):
        return None
    source = segments[index].get("src")
    return source if isinstance(source, dict) else None


def _segment_sha(segments: Sequence[dict[str, Any]], index: int) -> str | None:
    if index >= len(segments):
        return None
    sha = segments[index].get("sha")
    return sha if isinstance(sha, str) else None


def _baseline(
    recent: Sequence[dict[str, Any]],
    position: int,
    call: dict[str, Any],
    fingerprints: dict[str, Any],
    *,
    index: int,
    sha_after: str | None,
) -> list[dict[str, Any]]:
    """分叉的对照侧段列表——「改前」那一版是谁。

    `_find_ref` 只接受本次段列表的**前缀**（或完全相同）作参照，因此参照在自身长度范围内
    与本次逐段同值：`index < len(ref)` 时它的 sha 恒等于 `sha_after`（拿它当 `sha_before`
    只会重复 `sha_after`，解释不了「这里变了」），`index >= len(ref)` 时它在该位点上没有段。
    能解释这一行的是记录时分叉真正相对的那条调用（`recent[position + 1]`）；参照只在邻座
    已经滚出账本、或邻座在该位点没有段时兜底。
    """
    neighbour = position + 1 if position + 1 < len(recent) else None
    latest = (
        _segments_of(fingerprints, recent[neighbour].get("fp"))
        if neighbour is not None
        else []
    )
    reference = _segments_of(fingerprints, call.get("ref_fp"))
    if _explains(latest, index, sha_after):
        return latest
    if _explains(reference, index, sha_after):
        return reference
    return latest or reference


def _explains(
    segments: Sequence[dict[str, Any]], index: int, sha_after: str | None
) -> bool:
    """这一侧在该位点上有段、且与本次不同（能当「改前」的证据）。"""
    sha = _segment_sha(segments, index)
    return sha is not None and sha != sha_after


def _ref_ts(
    recent: Sequence[dict[str, Any]],
    position: int,
    ref_fp: Any,
    *,
    fallback: int,
) -> str | None:
    """对照侧调用的时刻。

    `ref_fp` 记在案时只认它自己的那条调用（同指纹）；它已经滚出环形账本 → `None`——
    拿邻居的 ts 冒充参照会把「什么时候变的」指向一条无关的调用。没有参照（前缀被外部改动
    打断）时对照侧就是邻居，那正是分叉相对的对象，ts 是准确值。
    """
    if isinstance(ref_fp, str):
        for index in range(position + 1, len(recent)):
            if recent[index].get("fp") == ref_fp:
                return _ts_of(recent[index])
        return None
    if fallback < len(recent):
        return _ts_of(recent[fallback])
    return None


def _ts_of(call: dict[str, Any]) -> str | None:
    ts = call.get("ts")
    return ts if isinstance(ts, str) else None


def _blame(
    call: dict[str, Any],
    fork: CacheFork,
    recent: Sequence[dict[str, Any]],
    position: int,
) -> tuple[str | None, bool | None]:
    """派生归因（contract §4 的 blame 表；不落盘，读取侧现算）。"""
    verdict = call.get("verdict") or ""
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
        if _compact_between(recent, position, call.get("ref_fp")):
            return "compact", False
        return "composition", False
    return "composition", False


def _compact_between(
    recent: Sequence[dict[str, Any]], position: int, ref_fp: Any
) -> bool:
    """参照调用与本次调用之间是否有过压缩调用（contract §4 的 compact 归因判据）。"""
    if not isinstance(ref_fp, str):
        return False
    ref_position = None
    for index in range(position + 1, len(recent)):
        if recent[index].get("fp") == ref_fp:
            ref_position = index
            break
    if ref_position is None:
        return False
    return any(
        recent[index].get("purpose") == PURPOSE_COMPACT
        for index in range(position + 1, ref_position)
    )


def _summary(calls: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """汇总（contract §6）：三个 rate 的分母都是 Σ prompt；分母为 0 时全 None。"""
    prompt_total = sum(_int(call.get("prompt")) for call in calls)
    cached_total = sum(_int(call.get("cached")) for call in calls)
    expected_total = sum(
        _int(call.get("expected"))
        for call in calls
        if isinstance(call.get("expected"), int)
    )
    debt_total = sum(
        max(0, call["expected"] - _int(call.get("cached")))
        for call in calls
        if isinstance(call.get("expected"), int)
        and call.get("verdict")
        in (VERDICT_PROVIDER_UNDERDELIVERED, VERDICT_BLOCK_GRANULARITY)
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


def _int(value: Any) -> int:
    return value if isinstance(value, int) and value > 0 else 0


def build_cache_report(
    session_id: str, data: Any, *, limit: int = 20
) -> dict[str, Any]:
    """端点入口：盘上的原始账本 → 响应体（`data=None` / 损坏 / 会话不存在按空账本投影）。

    与 `build_report` 的分工：本函数负责「原始数据 → 账本」的容错规整，调用方
    （路由）因此不需要知道账本版本与损坏语义——空账本照样是 200 的形状。
    """
    return build_report(
        normalize_ledger(data, session_id), session_id=session_id, limit=limit
    )


# ── 读写口 ────────────────────────────────────────────────────────────────


_RECORD_LOCK = threading.Lock()
"""记账的 read-modify-write 临界区。

记账跑在线程池里（`ContextManager.observe_cache_call`），同一会话的两路调用（主调用与
后台增量压缩）可能真并行——没有这把锁就会双双读到同一份账本再互相覆盖（静默丢记录）。
进程级单锁即可：记账是低频动作（每次模型调用一次），跨会话争用可忽略。
"""


class CacheLedger:
    """一个会话的账本读写口。

    每次记录都重新读盘（不缓存内存态）：`switch_template` 会重建 `ContextManager`，内存态
    会出现两个残影；一次调用读写几十 KB JSON 的代价远小于一次模型调用。
    """

    def __init__(self, aux: Any, session_id: str) -> None:
        self._aux = aux
        self._session_id = session_id

    def load(self) -> dict[str, Any]:
        """当前账本（读失败 = 空账本，绝不抛）。"""
        try:
            raw = self._aux.read_aux(CACHE_LEDGER_KEY)
        except Exception as e:
            log.warning(f"cache ledger unreadable (session={self._session_id}): {e}")
            return empty_ledger(self._session_id)
        return normalize_ledger(raw, self._session_id)

    def record(
        self,
        segments: Sequence[CachePart],
        *,
        purpose: str,
        model: str,
        provider: str,
        prompt: int,
        cached: int,
    ) -> CacheObservation | None:
        """记一次调用（best-effort：任何失败只 WARN，返回 None）。"""
        if not segments:
            return None
        try:
            with _RECORD_LOCK:
                data, observation = record_call(
                    self.load(),
                    segments,
                    purpose=purpose,
                    model=model,
                    provider=provider,
                    prompt=prompt,
                    cached=cached,
                )
                self._aux.write_aux(CACHE_LEDGER_KEY, data)
            return observation
        except Exception as e:
            log.warning(f"cache ledger record failed (session={self._session_id}): {e}")
            return None

    def report(self, limit: int = 20) -> dict[str, Any]:
        """端点投影（永不抛）。"""
        return build_report(self.load(), session_id=self._session_id, limit=limit)
