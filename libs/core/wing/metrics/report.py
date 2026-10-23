# wing/metrics/report.py — 用量聚合的查询编排（时间窗口 + 维度 → wire dict）
"""``/api/usage`` 的唯一编排层：**校验 → 取数 → 投影**（不写 SQL、不碰传输层）。

各个维度是同一组度量的不同切法，因此共用一条「先摊平、再分桶」的骨架
（见 :mod:`wing.metrics.store`）：

- ``day``：稠密（窗口内每天都有一行，空天全 0）——热力图直接画，消费方不做日历数学；
- ``origin``：恰好两行、顺序固定（``me`` / ``agents``），归属按目录里的
  ``parent_session_id``；目录里没有的会话按 ``me`` 计（不知道就当是自己开的）；
- ``session`` / ``workspace`` / ``agent``：都来自 per-session 聚合，按桶相加；
- ``model`` / ``tool``：只有一边的度量可归因（工具行没有 model 列、模型行没有工具名），
  另一边恒为 0——可归因性表见 ``docs/dev/usage.md``。

``totals`` 永远由独立的全量查询给出（不依赖被 ``limit`` 截断的行），因此消费方的
TOTAL 行永远可对账；行和 ≠ totals 只发生在「不可归因」与「被截断」两种情形。
"""

from __future__ import annotations

from dataclasses import replace
from datetime import date, datetime, timedelta
from typing import Any, Literal

from wing.common.utils import validate_session_id
from wing.config import Config, get_config, resolve_model_display_name

from .measures import Measures, Row, Window, sum_measures
from .store import MetricsStore

#: ``group_by`` 的取值域（顺序即文档与报错文案的顺序）。
GROUP_BY_VALUES: tuple[str, ...] = (
    "day",
    "model",
    "session",
    "tool",
    "workspace",
    "agent",
    "origin",
)

#: ``origin`` 的两个固定行（顺序是契约：``me`` 在前）。
ORIGIN_VALUES: tuple[str, ...] = ("me", "agents")

#: 缺省窗口跨度（12 周）。``since`` 缺省时 = ``until`` 往前这么多天。
DEFAULT_RANGE_DAYS = 84

#: ``limit`` 的上界（与端点的 ``Query(ge=1, le=200)`` 同值）。
MAX_LIMIT = 200


def usage_report(
    *,
    since: str | None = None,
    until: str | None = None,
    group_by: str = "day",
    session_id: str | None = None,
    tree: bool = False,
    limit: int = 20,
) -> dict[str, Any]:
    """聚合窗口内的用量事实，返回**线格式**（``GET /api/usage`` 的响应体）。

    Args:
        since: ``YYYY-MM-DD`` 或字面量 ``all``（= 有记录的第一天）；缺省 = ``until``
            往前 :data:`DEFAULT_RANGE_DAYS` 天。
        until: ``YYYY-MM-DD``；缺省 = 今天（网关本地时区）。
        group_by: :data:`GROUP_BY_VALUES` 之一。
        session_id: 把范围收敛到某会话（``tree`` 时再带上全部子孙）。
        tree: 必须配 ``session_id``（否则 400）。
        limit: 排行类维度的行数上限（1..:data:`MAX_LIMIT`）。

    Raises:
        ValueError: 参数语义非法（未知维度 / 日期格式 / ``since > until`` /
            ``tree`` 没配 ``session_id`` / ``limit`` 越界 / 会话 id 词法非法）。
        LookupError: ``session_id`` 在库里没有任何痕迹（目录行与审计行都没有）。
    """
    if group_by not in GROUP_BY_VALUES:
        raise ValueError(
            f"unknown group_by: {group_by!r}; available: {', '.join(GROUP_BY_VALUES)}"
        )
    if not 1 <= limit <= MAX_LIMIT:
        raise ValueError(f"limit must be between 1 and {MAX_LIMIT} (got {limit})")
    if tree and session_id is None:
        raise ValueError("tree requires session_id")
    scope = validate_session_id(session_id) if session_id is not None else None

    metrics = MetricsStore()
    window = _resolve_window(since, until, scope=scope, tree=tree, metrics=metrics)
    if (
        scope is not None
        and metrics.database().ready
        and not metrics.has_usage_record(scope)
    ):
        # 0 是一个读数，「打错了 id」不是——静默给全 0 会让调用方以为这个会话没花钱。
        # 库降级时不判：读不到 ≠ 不存在（端点照常 200 + 全零）。
        raise LookupError(f"session not found: {scope!r}")

    rows, truncated = _rows(metrics, window, group_by, limit)
    totals = replace(
        metrics.totals(window), sessions_created=metrics.created_total(window)
    )
    return {
        "range": window.wire(),
        "group_by": group_by,
        "totals": totals.wire(),
        "rows": [row.wire() for row in rows],
        "truncated": truncated,
    }


# ── 窗口解析 ─────────────────────────────────────


def _resolve_window(
    since: str | None,
    until: str | None,
    *,
    scope: str | None,
    tree: bool,
    metrics: MetricsStore,
) -> Window:
    """把两个日期字符串解析成闭区间。

    ``since=all`` 取库里最早的一天（= 稠密行从有数据处开始）。时钟回拨 / 向西跨时区
    会留下「记录时刻晚于读端今天」的行，所以 ``all`` 取 ``min(最早一天, until)``：
    这条逃生路径（「范围内没活动 → try --since all」）绝不变成 400。

    显式 ``since`` 与缺省窗口都按请求原样收敛：``day`` 的行数 = 跨度天数，是消费方
    （热力图要固定格子）的语义，不因数据少而缩短。
    """
    end = _parse_day(until, field="until") if until is not None else date.today()
    if since is None:
        start = end - timedelta(days=DEFAULT_RANGE_DAYS)
    elif since.strip().lower() == "all":
        earliest = metrics.earliest_day()
        start = min(_parse_day(earliest, field="since"), end) if earliest else end
    else:
        start = _parse_day(since, field="since")
    if start > end:
        raise ValueError(
            f"since ({start.isoformat()}) is after until ({end.isoformat()})"
        )
    return Window(since=start, until=end, session_id=scope, tree=tree)


def _parse_day(raw: str, *, field: str) -> date:
    """``YYYY-MM-DD`` → 日期。只认这一种形态（``fromisoformat`` 还接受紧凑写法）。"""
    try:
        return datetime.strptime(raw.strip(), "%Y-%m-%d").date()
    except ValueError:
        raise ValueError(f"invalid {field}: {raw!r} (expect YYYY-MM-DD)") from None


# ── 各维度的行投影 ────────────────────────────────


def _rows(
    metrics: MetricsStore, window: Window, group_by: str, limit: int
) -> tuple[list[Row], bool]:
    """按维度出行；第二个返回值是「行被 ``limit`` 截断了吗」。"""
    if group_by == "day":
        return _day_rows(metrics, window), False
    if group_by == "origin":
        return _origin_rows(metrics, window), False
    if group_by == "model":
        return _cut(_rank(_model_rows(metrics, window), by_tokens=True), limit)
    if group_by == "tool":
        return _cut(_rank(_tool_rows(metrics, window), by_tokens=False), limit)
    if group_by == "session":
        return _session_rows(metrics, window, limit)
    dimension = "workspace" if group_by == "workspace" else "agent"
    rows = _key_rows(metrics, window, dimension)
    return _cut(_rank(rows, by_tokens=True), limit)


def _day_rows(metrics: MetricsStore, window: Window) -> list[Row]:
    """稠密的一天一行（窗口内每一天都在，空天全 0；升序）。"""
    measures = metrics.by_day(window)
    created = metrics.created_by_day(window)
    return [
        Row(
            key=day,
            measures=replace(
                measures.get(day, Measures()), sessions_created=created.get(day, 0)
            ),
        )
        for day in window.days()
    ]


def _origin_rows(metrics: MetricsStore, window: Window) -> list[Row]:
    """恰两行、顺序固定：``me``（根会话）在前、``agents``（有父的会话）在后。"""
    buckets = metrics.by_session(window)
    created_me, created_agents = metrics.created_by_origin(window)
    me = sum_measures([b.measures for b in buckets if b.parent_session_id is None])
    agents = sum_measures(
        [b.measures for b in buckets if b.parent_session_id is not None]
    )
    return [
        Row(key=ORIGIN_VALUES[0], measures=replace(me, sessions_created=created_me)),
        Row(
            key=ORIGIN_VALUES[1],
            measures=replace(agents, sessions_created=created_agents),
        ),
    ]


def _session_rows(
    metrics: MetricsStore, window: Window, limit: int
) -> tuple[list[Row], bool]:
    """按会话排行（token 降序）；作用域的自身行**不参与截断**（它是锚点）。"""
    created = metrics.created_by_session(window)
    rows = [
        Row(
            key=bucket.session_id,
            parent_session_id=bucket.parent_session_id,
            measures=replace(
                bucket.measures,
                sessions_created=created.get(bucket.session_id, 0),
            ),
        )
        for bucket in metrics.by_session(window)
    ]
    kept, truncated = _cut(_rank(rows, by_tokens=True), limit)
    if window.session_id is not None and all(r.key != window.session_id for r in kept):
        anchor = next(
            (row for row in rows if row.key == window.session_id),
            Row(
                key=window.session_id,
                measures=Measures(sessions_created=created.get(window.session_id, 0)),
            ),
        )
        kept.append(anchor)
    return kept, truncated


def _key_rows(
    metrics: MetricsStore, window: Window, dimension: Literal["workspace", "agent"]
) -> list[Row]:
    """按 ``workspace`` / ``agent`` 分桶（空值 → 空串键）。

    行 = 「有审计活动的会话所在的桶」∪「窗口内建过会话的桶」：只在窗口内新建、
    还没有任何活动的会话也占一行（只有 ``sessions_created``），否则
    ``--by workspace`` 会在「刚建的会话」上静默少一行。
    """
    created = (
        metrics.created_by_workspace(window)
        if dimension == "workspace"
        else metrics.created_by_template(window)
    )
    groups: dict[str, list[Measures]] = {}
    for bucket in metrics.by_session(window):
        key = (
            bucket.workspace if dimension == "workspace" else bucket.template_name
        ) or ""
        groups.setdefault(key, []).append(bucket.measures)
    return [
        Row(
            key=key,
            measures=replace(
                sum_measures(groups.get(key, [])), sessions_created=created.get(key, 0)
            ),
        )
        for key in groups.keys() | created.keys()
    ]


def _model_rows(metrics: MetricsStore, window: Window) -> list[Row]:
    """按 ``provider/model`` 排行；``label`` = 配置里的展示名（解析不出为 None）。"""
    config = get_config()
    return [
        Row(
            key=f"{bucket.provider}/{bucket.model}",
            label=_display_name(config, bucket.provider, bucket.model),
            measures=bucket.measures,
        )
        for bucket in metrics.by_model(window)
    ]


def _tool_rows(metrics: MetricsStore, window: Window) -> list[Row]:
    """按工具名排行（排序键见 :func:`_rank`）。"""
    return [
        Row(key=bucket.tool_name, measures=bucket.measures)
        for bucket in metrics.by_tool(window)
    ]


def _display_name(config: Config, provider: str, model: str) -> str | None:
    """``(provider, 调用名)`` → 展示名；未声明 / 已从配置删除 → None。

    ``audit_llm_call.model`` 存的是**调用名**（``ModelSpec.name``），因此直接按
    provider 内的调用名查声明——与「展示名解析的唯一出口」
    （``resolve_model_display_name``）同一份规则，不另做名字启发式。
    """
    try:
        provider_cfg = config.get_provider(provider)
    except ValueError:
        return None
    return resolve_model_display_name(provider_cfg, model)


# ── 排序与截断 ───────────────────────────────────


def _rank(rows: list[Row], *, by_tokens: bool) -> list[Row]:
    """全序排序：主键降序（总 token / 工具调用数）、并列按 key 升序。

    ``tool`` 维度按 ``tool_calls`` 排（token 对一次工具调用没有意义），其余维度
    按 ``prompt_tokens + completion_tokens``——这是「谁最贵」的同一把尺子。全序是
    契约：并列不按库内顺序漂移，消费方据此做跨次对比。
    """
    if by_tokens:
        return sorted(
            rows,
            key=lambda row: (
                -(row.measures.prompt_tokens + row.measures.completion_tokens),
                row.key,
            ),
        )
    return sorted(rows, key=lambda row: (-row.measures.tool_calls, row.key))


def _cut(rows: list[Row], limit: int) -> tuple[list[Row], bool]:
    return rows[:limit], len(rows) > limit
