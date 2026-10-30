#!/usr/bin/env python3
"""人读报告：结论 + 档位表格 + rig 质量信号 + 判据原文 + 复现命令（只读 ``run.json``）。

报告与机器可读产物必须同源，否则两处数字迟早对不上。缺数据一律显示 ``—``
（与 JSON 里的 ``null`` 对应，绝不写 0）。

判据**在这里重算**（``verdict.evaluate_run``），不读 ``levels[i].verdict``：那份投影是
给机器读的，而报告还要能处理「手写的 / 被污染的 ``run.json``」——没有 ``verdict`` 字段
的输入同样必须被标 ``low_confidence``，不能因为缺字段就当作「一切正常」。
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import Any

import verdict

MISSING = "—"
"""没有样本的占位符（对应 JSON 的 ``null``）。"""

NOTE_LIMIT = 400
"""报告里单条说明的字符上限（失败明细的完整文本在 ``levels[i].error``）。"""

LEVEL_COLUMNS: tuple[str, ...] = (
    "N",
    "wall s",
    "gw cpu",
    "gw rss MB peak / end",
    "gw fds",
    "delivery p50 / p99 / p999 ms",
    "turn p50 / p99 ms",
    "ev/s (chars/s)",
    "turns/s",
    "errors",
    "recycles",
    "verdict",
)

HYDRATION_COLUMNS: tuple[str, ...] = (
    "N",
    "restart ms",
    "subscribe → sync p50 / p99 / p999 ms",
    "sync / attempts",
    "first frame p50 / p99 ms",
    "history tokens",
    "seed ms",
)

RIG_COLUMNS: tuple[str, ...] = (
    "N",
    "provider cpu",
    "shard cpu (per shard)",
    "loadavg",
    "paired / dropped / content frames / turns (done + in flight)",
    "low conf",
)


def _num(value: Any, digits: int = 1) -> str:
    if value is None or isinstance(value, bool):
        return MISSING
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return f"{value:.{digits}f}"
    return str(value)


def _row(cells: Sequence[str]) -> str:
    return "| " + " | ".join(cells) + " |"


def _table(columns: Sequence[str], rows: Sequence[Sequence[str]]) -> list[str]:
    """表头 / 分隔 / 数据三段由**同一个列数**生成（GFM 要求三者一致）。"""
    return [
        _row(columns),
        "|" + "---|" * len(columns),
        *(_row(row) for row in rows),
    ]


def _triple(record: Mapping[str, Any], prefix: str) -> str:
    metrics = record.get("metrics") or {}
    parts = [
        _num(metrics.get(f"{prefix}.{label}_ms")) for label in ("p50", "p99", "p999")
    ]
    return MISSING if all(part == MISSING for part in parts) else " / ".join(parts)


def _duo(record: Mapping[str, Any], first: str, second: str) -> str:
    """``a (b)``：两个同量纲指标并排。

    相同也照样写两个：速率的三个口径（事件 / 字符 / 轮次）必须并排可见，
    「恰好相等」不是省略的理由（``--chunk=1`` 时事件数与字符数恒等）。
    """
    metrics = record.get("metrics") or {}
    left, right = _num(metrics.get(first)), _num(metrics.get(second))
    if right == MISSING:
        return left
    return f"{left} ({right})"


def _pair(record: Mapping[str, Any], first: str, second: str) -> str:
    metrics = record.get("metrics") or {}
    left, right = _num(metrics.get(first)), _num(metrics.get(second))
    return MISSING if left == MISSING and right == MISSING else f"{left} / {right}"


def _shard_cpu(record: Mapping[str, Any]) -> str:
    values = (record.get("rig") or {}).get("shard_cpu_cores") or []
    rendered = [_num(value, 2) for value in values]
    return ", ".join(rendered) if rendered else MISSING


def _verdict_cell(result: verdict.Verdict) -> str:
    if result.verdict is None:
        return MISSING
    if result.low_confidence:
        return f"{result.verdict} (low conf)"
    return result.verdict


def _level_rows(
    levels: Sequence[Mapping[str, Any]], verdicts: Sequence[verdict.Verdict]
) -> list[list[str]]:
    rows: list[list[str]] = []
    for record, result in zip(levels, verdicts):
        metrics = record.get("metrics") or {}
        rows.append(
            [
                str(record.get("sessions", MISSING)),
                _num(record.get("wall_s")),
                _num(metrics.get("gateway.cpu_cores"), 2),
                _pair(record, "gateway.rss_mb_peak", "gateway.rss_mb_end"),
                _num(metrics.get("gateway.fds_peak")),
                _triple(record, "delivery"),
                _pair(record, "turn.p50_ms", "turn.p99_ms"),
                _duo(record, "events_per_second_actual", "chars_per_second"),
                _num(metrics.get("turns_per_second"), 2),
                _num(metrics.get("errors")),
                _num(metrics.get("slow_client_recycles")),
                _verdict_cell(result),
            ]
        )
    return rows


def _hydration_rows(levels: Sequence[Mapping[str, Any]]) -> list[list[str]]:
    rows: list[list[str]] = []
    for record in levels:
        samples = record.get("samples") or {}
        history = record.get("history") or {}
        rows.append(
            [
                str(record.get("sessions", MISSING)),
                _num((record.get("gateway") or {}).get("restart_ms")),
                _triple(record, "hydration"),
                f"{_num(samples.get('sync_sessions'))} / "
                f"{_num(samples.get('hydration_attempts'))}",
                _pair(record, "first_frame.p50_ms", "first_frame.p99_ms"),
                _num(history.get("tokens_per_session")),
                _num(history.get("total_ms")),
            ]
        )
    return rows


def _has_hydration(levels: Sequence[Mapping[str, Any]]) -> bool:
    for record in levels:
        if (record.get("shape") or {}).get("window_mode") == "hydrate":
            return True
        if (record.get("gateway") or {}).get("restart_ms") is not None:
            return True
        if (record.get("metrics") or {}).get("hydration.p50_ms") is not None:
            return True
    return False


def _rig_rows(
    levels: Sequence[Mapping[str, Any]], verdicts: Sequence[verdict.Verdict]
) -> list[list[str]]:
    rows: list[list[str]] = []
    for record, result in zip(levels, verdicts):
        rig = record.get("rig") or {}
        samples = record.get("samples") or {}
        loadavg = rig.get("host_loadavg")
        rows.append(
            [
                str(record.get("sessions", MISSING)),
                _num(rig.get("provider_cpu_cores"), 2),
                _shard_cpu(record),
                ", ".join(f"{value:.2f}" for value in loadavg)
                if isinstance(loadavg, list)
                else MISSING,
                " / ".join(
                    [
                        _num(samples.get("count")),
                        _num(samples.get("dropped")),
                        _num((samples.get("delta_frames") or {}).get("content")),
                        _num(samples.get("turns")),
                    ]
                )
                + f" + {_num(samples.get('turns_in_flight'))}",
                f"{len(result.low_confidence_reasons)} hit(s)"
                if result.low_confidence
                else ("no" if result.low_confidence is not None else MISSING),
            ]
        )
    return rows


def _banners(run: Mapping[str, Any], verdicts: Sequence[verdict.Verdict]) -> list[str]:
    meta = run.get("meta") or {}
    lines: list[str] = []
    if meta.get("failures"):
        lines.append(
            f"> **失败档位**：{len(meta['failures'])} 个档位未跑完——"
            "**未测到**的指标是 `null`，已经测到的照常显示（详见该档 `error`）。"
        )
    unavailable = [
        record.get("sessions")
        for record in run.get("levels") or []
        if (record.get("samples") or {}).get("delivery_available") is False
        and not record.get("error")
    ]
    if unavailable:
        lines.append(
            f"> **投递延迟不可用**：{unavailable} 档的 `delivery.*` 为 `null` —— "
            "假 Provider 不提供逐帧发射时刻（`frame_emits`），无法把客户端到达时刻"
            "配到发射时钟上。其余指标不受影响。"
        )
    cap = verdict.effective_pool_cap(meta)
    pool = [
        record.get("sessions")
        for record, result in zip(run.get("levels") or [], verdicts)
        if any(
            reason.startswith(verdict.CRITERIA["low_confidence.pool"])
            for reason in result.low_confidence_reasons
        )
    ]
    if pool:
        pairs = ", ".join(f"N={sessions} > {cap}" for sessions in pool)
        lines.append(
            f"> **连接池上限：{pairs}** —— SSE 是长连接，超出的会话在本地连接池里排队等"
            "连接：这一档的窗口测的是**池**，不是网关能扛多少。要看网关容量，用更大的 "
            "`--provider-max-connections` 重跑（不传该旗标时用的是网关默认值）。"
        )
    low = [
        (record.get("sessions"), result.low_confidence_reasons)
        for record, result in zip(run.get("levels") or [], verdicts)
        if result.low_confidence
    ]
    if low:
        named = "; ".join(
            f"N={sessions}（{len(reasons)} 条）" for sessions, reasons in low
        )
        lines.append(
            f"> **低置信：{named}** —— 这几档的读数**不能回答容量问题**，结论**不作数**，"
            "只能当参考。逐条原因见下方「判据」一节。"
        )
    unjudged = [
        record.get("sessions")
        for record, result in zip(run.get("levels") or [], verdicts)
        if result.unjudged
    ]
    if unjudged:
        lines.append(
            f"> **判据未评估：{unjudged}** —— 这几档少判了判据（逐条原因见下方「判据」"
            "一节）。**未评估不等于没有退化**：缺的那条判据这轮没有给结论。"
        )
    unknown = [
        record.get("sessions")
        for record, result in zip(run.get("levels") or [], verdicts)
        if result.verdict == "unknown"
    ]
    if unknown:
        lines.append(
            f"> **无法判定**：{unknown} 档没有可用的基线档（最小 N 档未跑完），"
            "退化判据未评估——**不是**「没有退化」。"
        )
    return lines


def _notes(run: Mapping[str, Any]) -> list[str]:
    """全局说明 + 逐档说明（同一句话只出现一次：全局那条已经说了）。"""
    meta = run.get("meta") or {}
    global_notes = {str(note) for note in meta.get("notes") or []}
    lines = [f"- {_one_line(note)}" for note in sorted(global_notes)]
    for record in run.get("levels") or []:
        for note in record.get("notes") or []:
            if str(note) in global_notes:
                continue
            lines.append(f"- N={record.get('sessions')}: {_one_line(note)}")
    return lines


def _one_line(text: Any) -> str:
    """多行诊断折成一行并截断（markdown 列表项不允许裸续行，报告也不该被日志淹没）。"""
    folded = " ".join(str(text).split())
    return folded if len(folded) <= NOTE_LIMIT else folded[: NOTE_LIMIT - 1] + "…"


def _gateway_field(meta: Mapping[str, Any], key: str) -> str:
    value = (meta.get("gateway") or {}).get(key)
    return MISSING if value is None else str(value)


def _first_flag(
    levels: Sequence[Mapping[str, Any]],
    verdicts: Sequence[verdict.Verdict],
    flag: str,
) -> tuple[Mapping[str, Any], verdict.Verdict] | None:
    """第一个命中某 flag 的**可用**档位（阶梯按 N 升序给出）。

    低置信档位不算：它的用途是给出「第一处退化」，而报告同时把那一档标成「结论不作数」
    ——同一段结论里既引用它又否认它，读的人只能两句话都不信。
    """
    for record, result in zip(levels, verdicts):
        if flag in result.flags and not result.low_confidence:
            return record, result
    return None


def _conclusion(
    run: Mapping[str, Any], verdicts: Sequence[verdict.Verdict]
) -> list[str]:
    """一句话结论：速度轴（判据）与内存轴（投影）分开说，低置信点名。"""
    levels = run.get("levels") or []
    meta = run.get("meta") or {}
    host = meta.get("host") or {}
    lines: list[str] = ["## 结论", ""]
    finished = [record for record in levels if not record.get("error")]
    if not finished:
        lines.append("- 没有档位跑完，无法给结论（见下方失败档位与 `error`）。")
        return [*lines, ""]
    degraded = _first_flag(levels, verdicts, "degraded")
    capped = _first_flag(levels, verdicts, "capped")
    if degraded is None and capped is None:
        usable = [
            (record, result)
            for record, result in zip(levels, verdicts)
            if result.verdict == "ok" and not result.low_confidence
        ]
        if usable:
            top, result = max(usable, key=lambda pair: int(pair[0]["sessions"]))
            lines.append(
                f"- **速度轴**：可用档位里最大的 N={top.get('sessions')} 没有命中判据"
                f"（{_evaluated_axes(top, result)}）。"
            )
        else:
            lines.append(
                "- **速度轴**：没有可用档位（全部低置信或未跑完），判据结论不作数。"
            )
    else:
        parts: list[str] = []
        if degraded is not None:
            record, result = degraded
            reason = verdict.first_reason(result, "degraded") or result.reasons[0]
            parts.append(f"体感退化先出现在 N={record.get('sessions')}：{reason}")
        if capped is not None:
            record, result = capped
            reason = verdict.first_reason(result, "capped") or result.reasons[0]
            parts.append(
                f"撞到天花板（capped）的第一档是 N={record.get('sessions')}：{reason}"
            )
        lines.append("- **速度轴**：" + "；".join(parts) + "。")

    low = [
        (record.get("sessions"), result.low_confidence_reasons)
        for record, result in zip(levels, verdicts)
        if result.low_confidence
    ]
    lines.append(
        "- **内存轴**："
        + _memory_axis(
            levels,
            host,
            {int(sessions or 0) for sessions, _ in low},
        )
        + "。"
    )
    if low:
        lines.append(
            "- **低置信**："
            + "、".join(f"N={sessions}" for sessions, _ in low)
            + " 的读数**不能回答容量问题**（原因见顶部横幅与「判据」一节），"
            "上面这句结论**没有引用它们**。"
        )
    unjudged = [
        record.get("sessions")
        for record, result in zip(levels, verdicts)
        if result.unjudged
    ]
    if unjudged:
        kinds = sorted(
            {
                _unjudged_label(reason)
                for result in verdicts
                for reason in result.unjudged
            }
        )
        lines.append(
            f"- **判据未评估**：{unjudged} 档少判了判据（{'、'.join(kinds)}）——"
            "**未评估不等于没有退化**，逐条原因见「判据」一节。"
        )
    return [*lines, ""]


#: 未评估判据的中文名（只影响人读文案；判据原文照旧从 ``verdict.CRITERIA`` 取）。
UNJUDGED_LABELS: dict[str, str] = {
    "unjudged.turn": "轮次尾部",
    "unjudged.delivery": "投递延迟",
}


def _unjudged_label(reason: str) -> str:
    for key, label in UNJUDGED_LABELS.items():
        if reason.startswith(verdict.CRITERIA[key]):
            return label
    return "未评估"


def _evaluated_axes(record: Mapping[str, Any], result: verdict.Verdict) -> str:
    """「这一档实际评估了哪些判据」——只说测到的量，不替缺数据的判据下结论。"""
    metrics = record.get("metrics") or {}
    parts: list[str] = []
    p99 = metrics.get("delivery.p99_ms")
    parts.append(
        f"投递延迟 p99 {_num(p99)} ms" if p99 is not None else "投递延迟未测到"
    )
    if (record.get("shape") or {}).get("window_mode") == "hold":
        parts.append("本形态不驱动")
    else:
        turn_p99 = metrics.get("turn.p99_ms")
        parts.append(
            f"轮次 p99 {_num(turn_p99)} ms" if turn_p99 is not None else "轮次未收口"
        )
    parts.append(f"网关 {_num(metrics.get('gateway.cpu_cores'), 2)} 核")
    parts.append(f"fd 峰值 {_num(metrics.get('gateway.fds_peak'))}")
    parts.append(f"错误 {_num(metrics.get('errors'))}")
    return "；".join(parts)


def _memory_axis(
    levels: Sequence[Mapping[str, Any]],
    host: Mapping[str, Any],
    low_confidence: set[int],
) -> str:
    """内存轴的**投影**（不给阈值、不判档）：斜率 × 1000 会话是决策者要的形态。

    RSS 由采样器独立采集，不受低置信原因（rig 饱和 / 池排队 / 丢弃）影响，因此照常
    参与投影；但那一档的速度轴读数不作数，返回的话里要说清这一点。
    """
    sampled = [
        (int(record.get("sessions") or 0), record.get("metrics") or {})
        for record in levels
        if not record.get("error")
        and (record.get("metrics") or {}).get("gateway.rss_mb_end") is not None
    ]
    if not sampled:
        return "没有采到 RSS（未测到）"
    sampled.sort()
    top_n, top = sampled[-1]
    peak = _num(top.get("gateway.rss_mb_peak"))
    end = _num(top.get("gateway.rss_mb_end"))
    slope = top.get("gateway.rss_mb_slope_per_session")
    mem_gb = host.get("mem_gb")
    text = f"N={top_n} 时网关 RSS {peak} MB 峰值 / {end} MB 末次"
    if slope is None:
        return text + "（只有一个档位有 RSS，给不出每会话斜率）"
    projected = float(top.get("gateway.rss_mb_end") or 0.0) + float(slope) * max(
        0, 1000 - top_n
    )
    text += f"；相邻档斜率 {float(slope):.3f} MB/会话 → 1000 会话线性外推约 {projected:.0f} MB"
    if isinstance(mem_gb, (int, float)) and mem_gb > 0:
        text += f"（本机 {mem_gb:g} GB 的 {projected / (mem_gb * 1024) * 100:.0f}%）"
    if top_n in low_confidence:
        text += "；这一档的速度轴读数不作数，但 RSS 是独立采样"
    return text


def _criteria_section(
    levels: Sequence[Mapping[str, Any]], verdicts: Sequence[verdict.Verdict]
) -> list[str]:
    """判据原文（``verdict.CRITERIA`` 渲染，报告里不写第二份阈值）+ 逐档命中。"""
    lines = ["## 判据", "", "判档阈值只在 `verdict.py` 声明一处，下面是它的原文：", ""]
    lines.extend(f"- `{key}`：{text}" for key, text in verdict.CRITERIA.items())
    lines.append("")
    hits: list[str] = []
    for record, result in zip(levels, verdicts):
        if result.verdict is None and not result.low_confidence:
            hits.append(f"- N={record.get('sessions')}：`—`（档位未跑完）")
            continue
        if result.verdict == "ok" and not result.low_confidence:
            continue
        bits = [f"- N={record.get('sessions')}：`{_verdict_cell(result)}`"]
        bits.extend(f"  - {reason}" for reason in result.reasons)
        bits.extend(f"  - 低置信：{reason}" for reason in result.low_confidence_reasons)
        bits.extend(f"  - 判据未评估：{reason}" for reason in result.unjudged)
        hits.append("\n".join(bits))
    lines.extend(hits if hits else ["- 全部档位都没有命中判据。"])
    lines.append("")
    return lines


#: ``meta.args`` 里不是旗标的键（本次运行的计时校准值，不是输入参数）。
NON_FLAG_ARG_KEYS: frozenset[str] = frozenset({"frame_delay_ms", "sleep_overshoot_ms"})


def _reproduce(meta: Mapping[str, Any]) -> list[str]:
    """复现命令：``meta.scenario`` + ``meta.args`` 里的每个键回放成 ``--<key>``。

    旗标名与 ``meta.args`` 的键同名（``cli`` 侧的 argparse dest 就是这么派生的），
    所以这里不需要第二张旗标表；``--selftest`` 会把生成的命令喂回 ``cli.build_config``
    做一次往返比对，名字漂移会当场变成自测失败。
    """
    args = meta.get("args") or {}
    parts = ["uv run python scripts/capacity/run.py"]
    scenario = meta.get("scenario")
    if scenario is not None:
        parts.append(f"--scenario {scenario}")
    for key, value in args.items():
        if key in NON_FLAG_ARG_KEYS or value is None:
            continue
        if isinstance(value, (list, tuple)):
            value = ",".join(str(item) for item in value)
        parts.append(f"--{str(key).replace('_', '-')} {value}")
    return ["## 复现", "", "```bash", " ".join(parts), "```", ""]


def render(run: Mapping[str, Any]) -> str:
    """``run.json`` → markdown（结论 + 表格 + 质量信号 + 判据 + 复现命令 + 口径脚注）。"""
    meta = run.get("meta") or {}
    host = meta.get("host") or {}
    args = meta.get("args") or {}
    levels = run.get("levels") or []
    verdicts = verdict.evaluate_run(run)
    lines = [f"# capacity — {meta.get('scenario', MISSING)}", ""]
    banners = _banners(run, verdicts)
    lines.extend(banners)
    if banners:
        lines.append("")
    lines.extend(
        [
            f"- started: {meta.get('started_at', MISSING)}"
            f"（duration {_num(meta.get('duration_s'))} s）",
            f"- host: {host.get('cpus')} cpus · {_num(host.get('mem_gb'))} GB · "
            f"{host.get('platform', MISSING)} · `ulimit -n` {host.get('ulimit_n')}",
            f"- args: rate={args.get('rate')} events/s per session · chunk="
            f"{args.get('chunk')} char/event · warmup={args.get('warmup')} s · "
            f"dwell={args.get('dwell')} s · shards={args.get('shards')} · "
            f"subscribers={args.get('subscribers')} · backend={args.get('backend')}",
            "- gateway: "
            f"pid {_gateway_field(meta, 'pid')} · version "
            f"{_gateway_field(meta, 'version')} · commit "
            f"{_gateway_field(meta, 'commit')}",
            "",
        ]
    )
    lines.extend(_conclusion(run, verdicts))
    lines.extend(
        [
            "## 各档读数",
            "",
            *_table(LEVEL_COLUMNS, _level_rows(levels, verdicts)),
            "",
        ]
    )
    if _has_hydration(levels):
        lines.extend(
            [
                "## 水合与首帧（网关重启 / 订阅风暴）",
                "",
                "* `subscribe → sync` = 订阅请求发出 → `sync_session` 到达（读历史 + 拼快照 + 过 wire）；",
                "  `first frame` = 驱动 prompt 发出 → 首个正文增量。两段是**不同的**时间线，不合成一个「启动耗时」。",
                "",
                *_table(HYDRATION_COLUMNS, _hydration_rows(levels)),
                "",
            ]
        )
    lines.extend(
        [
            "## rig（质量信号，不进退化判据）",
            "",
            "客户端 / 发生器饱和时读数不可信：`provider cpu` 与 `shard cpu` 之和接近核数，"
            "说明这一档测的是 rig 而不是网关。",
            "",
            *_table(RIG_COLUMNS, _rig_rows(levels, verdicts)),
            "",
        ]
    )
    notes = _notes(run)
    if notes:
        lines.extend(["## notes", "", *notes, ""])
    lines.extend(_criteria_section(levels, verdicts))
    lines.extend(_reproduce(meta))
    lines.extend(
        [
            "## 口径",
            "",
            "- `gw cpu` = 采样窗口内网关进程 CPU 秒 / 窗口秒（首尾累计差）。",
            "- `delivery` = 客户端收到第 i 帧 − 假 Provider 发射第 i 帧（同机单调时钟）；"
            "**按通道**（`content`→`text`、`thinking`→`reasoning`、"
            "`tool_call`→`tool_call_stream`）再按下标配对，`meta` 帧不参与；"
            "同一 prompt 命中多个请求时按通道消歧。配不上的帧记 `dropped` 并在 "
            "`levels[i].samples.dropped_reasons` 分类。",
            "- `turn` = 发 prompt → `turn_result`（只取窗口内完成的轮次；"
            "窗口末仍在飞的那一轮只出投递延迟、不出轮次耗时）。",
            "- `ev/s` = 窗口内收到的**全部通道** delta 事件数 / 窗口秒；"
            "`chars/s` = 窗口内收到的正文增量**字符数** / 窗口秒（实测，不是 `ev/s × chunk` 反推）。"
            "`--rate` 只是注入参数，结论一律以这两个实测值为准。",
            "- `turns/s` = 窗口内完成的轮次 / 窗口秒——与 `ev/s` 一起把「每轮固定成本」和"
            "「每事件成本」分开（`cpu_cores / turns/s` 就是每轮成本）。",
            "- 百分位为 nearest-rank（p99 = 升序第 `ceil(0.99·n)` 个值）；"
            "完整口径、判读方法与各形态回答什么问题见 `scripts/capacity/README.md`。",
            "- `—` / `null` = 没有样本，**不是 0**。",
            "",
        ]
    )
    return "\n".join(lines)


def summary_line(record: Mapping[str, Any]) -> str:
    """一档的一行摘要（CLI 退出前打在 stderr 上）。"""
    metrics = record.get("metrics") or {}
    return (
        f"N={record.get('sessions')}: gw_cpu={_num(metrics.get('gateway.cpu_cores'), 2)} "
        f"cores, rss_peak={_num(metrics.get('gateway.rss_mb_peak'))}MB, "
        f"fds_peak={_num(metrics.get('gateway.fds_peak'))}, "
        f"delivery_p99={_num(metrics.get('delivery.p99_ms'))}ms, "
        f"turn_p50={_num(metrics.get('turn.p50_ms'))}ms, "
        f"ev/s={_num(metrics.get('events_per_second_actual'))}, "
        f"turns/s={_num(metrics.get('turns_per_second'), 2)}, "
        f"errors={_num(metrics.get('errors'))}"
    )
