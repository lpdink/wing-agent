#!/usr/bin/env python3
"""人读报告（本步骤只做最小表格；判据 / 拐点 / 完整分析由下一步接手）。

渲染**只读** ``run.json``，不重算任何指标——报告与机器可读产物同源，否则两处
数字迟早对不上。缺数据一律显示 ``—``（与 JSON 里的 ``null`` 对应，绝不写 0）。
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import Any

MISSING = "—"
"""没有样本的占位符（对应 JSON 的 ``null``）。"""


def _num(value: Any, digits: int = 1) -> str:
    if value is None or isinstance(value, bool):
        return MISSING
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return f"{value:.{digits}f}"
    return str(value)


def _triple(record: Mapping[str, Any], prefix: str) -> str:
    metrics = record.get("metrics") or {}
    parts = [
        _num(metrics.get(f"{prefix}.{label}_ms")) for label in ("p50", "p99", "p999")
    ]
    return MISSING if all(part == MISSING for part in parts) else " / ".join(parts)


def _pair(record: Mapping[str, Any], first: str, second: str) -> str:
    metrics = record.get("metrics") or {}
    left, right = _num(metrics.get(first)), _num(metrics.get(second))
    return MISSING if left == MISSING and right == MISSING else f"{left} / {right}"


def _gateway_field(meta: Mapping[str, Any], key: str) -> str:
    value = (meta.get("gateway") or {}).get(key)
    return MISSING if value is None else str(value)


def _shard_cpu(record: Mapping[str, Any]) -> str:
    values = (record.get("rig") or {}).get("shard_cpu_cores") or []
    rendered = [_num(value, 2) for value in values]
    return ", ".join(rendered) if rendered else MISSING


def _level_rows(levels: Sequence[Mapping[str, Any]]) -> list[str]:
    rows = [
        "| N | wall s | gw cpu | gw rss MB peak / end | gw fds | delivery p50 / p99 / p999 ms "
        "| turn p50 / p99 ms | tok/s | errors | recycles |",
        "|" + "---|" * 11,
    ]
    for record in levels:
        metrics = record.get("metrics") or {}
        rows.append(
            "| {n} | {wall} | {cpu} | {rss} | {fds} | {delivery} | {turn} | {tps} | "
            "{errors} | {recycles} |".format(
                n=record.get("sessions"),
                wall=_num(record.get("wall_s")),
                cpu=_num(metrics.get("gateway.cpu_cores"), 2),
                rss=_pair(record, "gateway.rss_mb_peak", "gateway.rss_mb_end"),
                fds=_num(metrics.get("gateway.fds_peak")),
                delivery=_triple(record, "delivery"),
                turn=_pair(record, "turn.p50_ms", "turn.p99_ms"),
                tps=_num(metrics.get("tokens_per_second_actual")),
                errors=_num(metrics.get("errors")),
                recycles=_num(metrics.get("slow_client_recycles")),
            )
        )
    return rows


def _rig_rows(levels: Sequence[Mapping[str, Any]]) -> list[str]:
    rows = [
        "| N | provider cpu | shard cpu (per shard) | loadavg | samples (paired / dropped / frames) |",
        "|---|---|---|---|---|",
    ]
    for record in levels:
        rig = record.get("rig") or {}
        samples = record.get("samples") or {}
        loadavg = rig.get("host_loadavg")
        rows.append(
            "| {n} | {provider} | {shards} | {load} | {paired} / {dropped} / {frames} |".format(
                n=record.get("sessions"),
                provider=_num(rig.get("provider_cpu_cores"), 2),
                shards=_shard_cpu(record),
                load=", ".join(f"{value:.2f}" for value in loadavg)
                if isinstance(loadavg, list)
                else MISSING,
                paired=_num(samples.get("count")),
                dropped=_num(samples.get("dropped")),
                frames=_num(samples.get("text_frames")),
            )
        )
    return rows


def _banners(run: Mapping[str, Any]) -> list[str]:
    meta = run.get("meta") or {}
    lines: list[str] = []
    if meta.get("failures"):
        lines.append(
            f"> **失败档位**：{len(meta['failures'])} 个档位未完成，"
            "对应行指标全为 `null`（详见该档 `error`）。"
        )
    unavailable = [
        record.get("sessions")
        for record in run.get("levels") or []
        if (record.get("samples") or {}).get("delivery_available") is False
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
    global_notes = [str(note) for note in meta.get("notes") or []]
    lines = [f"- {note}" for note in global_notes]
    for record in run.get("levels") or []:
        for note in record.get("notes") or []:
            if str(note) in global_notes:
                continue
            lines.append(f"- N={record.get('sessions')}: {note}")
    return lines


def render(run: Mapping[str, Any]) -> str:
    """``run.json`` → markdown（整份报告；本步骤只含表格 + 口径脚注）。"""
    meta = run.get("meta") or {}
    host = meta.get("host") or {}
    args = meta.get("args") or {}
    levels = run.get("levels") or []
    lines = [
        f"# capacity — {meta.get('scenario', MISSING)}",
        "",
    ]
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
            *_level_rows(levels),
            "",
            "## rig（质量信号，不进退化判据）",
            "",
            "客户端 / 发生器饱和时读数不可信：`provider cpu` 与 `shard cpu` 之和接近核数，"
            "说明这一档测的是 rig 而不是网关。",
            "",
            *_rig_rows(levels),
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
            "按帧下标严格配对，配不上的样本记 `dropped`。",
            "- `turn` = 发 prompt → `turn_result`（只取窗口内发起且完成的轮次）。",
            "- `tok/s` = 窗口内收到的 `text` 事件数 / 窗口秒（1 帧 = 1 token 是本形态的约定）。",
            "- 百分位为 nearest-rank（p99 = 升序第 `ceil(0.99·n)` 个值）。",
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


__all__ = ["MISSING", "render", "summary_line"]
