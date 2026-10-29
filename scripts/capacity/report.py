#!/usr/bin/env python3
"""人读报告：档位表格 + rig 质量信号 + 口径脚注（只读 ``run.json``，不重算指标）。

报告与机器可读产物必须同源，否则两处数字迟早对不上。缺数据一律显示 ``—``
（与 JSON 里的 ``null`` 对应，绝不写 0）。判据与拐点分析的渲染不在这里。
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import Any

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
    "tok/s (ev/s)",
    "errors",
    "recycles",
)

RIG_COLUMNS: tuple[str, ...] = (
    "N",
    "provider cpu",
    "shard cpu (per shard)",
    "loadavg",
    "paired / dropped / content frames / turns (done + in flight)",
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
    """``a (b)``：两个同量纲指标并排（相同则只写一个）。"""
    metrics = record.get("metrics") or {}
    left, right = _num(metrics.get(first)), _num(metrics.get(second))
    if left == right or right == MISSING:
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


def _level_rows(levels: Sequence[Mapping[str, Any]]) -> list[list[str]]:
    rows: list[list[str]] = []
    for record in levels:
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
                _duo(record, "tokens_per_second_actual", "events_per_second_actual"),
                _num(metrics.get("errors")),
                _num(metrics.get("slow_client_recycles")),
            ]
        )
    return rows


def _rig_rows(levels: Sequence[Mapping[str, Any]]) -> list[list[str]]:
    rows: list[list[str]] = []
    for record in levels:
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
            ]
        )
    return rows


def _banners(run: Mapping[str, Any]) -> list[str]:
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


def render(run: Mapping[str, Any]) -> str:
    """``run.json`` → markdown（表格 + 质量信号 + 口径脚注）。"""
    meta = run.get("meta") or {}
    host = meta.get("host") or {}
    args = meta.get("args") or {}
    levels = run.get("levels") or []
    lines = [f"# capacity — {meta.get('scenario', MISSING)}", ""]
    banners = _banners(run)
    lines.extend(banners)
    if banners:
        lines.append("")
    lines.extend(
        [
            f"- started: {meta.get('started_at', MISSING)}"
            f"（duration {_num(meta.get('duration_s'))} s）",
            f"- host: {host.get('cpus')} cpus · {_num(host.get('mem_gb'))} GB · "
            f"{host.get('platform', MISSING)} · `ulimit -n` {host.get('ulimit_n')}",
            f"- args: rate={args.get('rate')} tok/s per session · warmup="
            f"{args.get('warmup')} s · dwell={args.get('dwell')} s · shards="
            f"{args.get('shards')} · subscribers={args.get('subscribers')}",
            "- gateway: "
            f"pid {_gateway_field(meta, 'pid')} · version "
            f"{_gateway_field(meta, 'version')} · commit "
            f"{_gateway_field(meta, 'commit')}",
            "",
            "## 各档读数",
            "",
            *_table(LEVEL_COLUMNS, _level_rows(levels)),
            "",
            "## rig（质量信号，不进退化判据）",
            "",
            "客户端 / 发生器饱和时读数不可信：`provider cpu` 与 `shard cpu` 之和接近核数，"
            "说明这一档测的是 rig 而不是网关。",
            "",
            *_table(RIG_COLUMNS, _rig_rows(levels)),
            "",
        ]
    )
    notes = _notes(run)
    if notes:
        lines.extend(["## notes", "", *notes, ""])
    lines.extend(
        [
            "## 口径",
            "",
            "- `gw cpu` = 采样窗口内网关进程 CPU 秒 / 窗口秒（首尾累计差）。",
            "- `delivery` = 客户端收到第 i 帧 − 假 Provider 发射第 i 帧（同机单调时钟）；"
            "**按通道**（`content`→`text`、`thinking`→`reasoning`、"
            "`tool_call`→`tool_call_stream`）再按下标配对，`meta` 帧不参与；"
            "配不上的帧记 `dropped` 并在 `levels[i].samples.dropped_reasons` 分类。",
            "- `turn` = 发 prompt → `turn_result`（只取窗口内完成的轮次；"
            "窗口末仍在飞的那一轮只出投递延迟、不出轮次耗时）。",
            "- `tok/s` = 窗口内收到的 `content` delta 事件数 / 窗口秒（1 帧 = 1 token）；"
            "`ev/s` = 全部通道的 delta 事件数 / 窗口秒。**标称 `--rate` 只是注入参数**，"
            "结论一律以这两个实测值为准（报告里两者相同说明这个形态只有 content 通道）。",
            "- `turn` 为 `—` 说明窗口内没有完成的轮次：`pure_stream` 每会话一轮、轮长覆盖"
            " warmup+dwell，所以窗口里只含每事件成本，不含每轮固定成本。",
            "- `paired / dropped` 是投递延迟配对的帧数：`dropped` 由 "
            "`levels[i].samples.dropped_reasons` 分类（`outside_window` = 跨窗口起点的"
            "预热段、`in_flight_tail` = 窗口末仍在传输的帧）。",
            "- 百分位为 nearest-rank（p99 = 升序第 `ceil(0.99·n)` 个值）；"
            "完整口径与判读方法见 `scripts/capacity/README.md`。",
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
        f"tok/s={_num(metrics.get('tokens_per_second_actual'))}, "
        f"errors={_num(metrics.get('errors'))}"
    )
