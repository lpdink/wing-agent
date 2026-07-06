# wing/metrics_registry/experimental.py
"""BetterEdit 工具实验审计 handler。

追踪 BetterEdit 工具使用情况，特别是 [upto] 锚点模式的使用率和成功率。
审计数据保存到全局 ~/.wing/metrics_experimental.json，不区分 session。
"""

from __future__ import annotations

from pydantic import BaseModel

from wing.common.logger import log
from wing.config import get_wing_home
from wing.event import ToolCallResultEvent
from wing.metrics_registry.core import (
    _atomic_write_json,
    _read_metrics_json,
    metrics_registry,
)

# Marker for anchored edit
UPTO_MARKER = "[upto]"


# ============================================================
# Schema — BetterEdit 实验审计 entry
# ============================================================


class BetterEditMetricsEntry(BaseModel):
    """BetterEdit 工具调用聚合 entry。"""

    times: int = 0
    with_upto_times: int = 0
    upto_error_times: int = 0
    error_times: int = 0

    def aggregate(self, event: ToolCallResultEvent) -> BetterEditMetricsEntry:
        """累加一次工具调用结果。返回 self（就地修改）。"""
        self.times += 1

        old_block = event.tool_args.get("old_block", "")
        has_upto = UPTO_MARKER in old_block

        if has_upto:
            self.with_upto_times += 1

        if not event.tool_success:
            self.error_times += 1
            if has_upto:
                self.upto_error_times += 1

        return self


class BetterEditMetrics(BaseModel):
    """BetterEdit 全局审计容器。key = model:BetterEdit。"""

    entries: dict[str, BetterEditMetricsEntry] = {}

    @classmethod
    def from_raw(cls, data: dict) -> BetterEditMetrics:
        raw = data.get("tool_calls", {})
        entries = {}
        for key, val in raw.items():
            entries[key] = BetterEditMetricsEntry.model_validate(val)
        return cls(entries=entries)

    def to_raw(self) -> dict:
        return {k: v.model_dump() for k, v in self.entries.items()}


# ============================================================
# Handler
# ============================================================


@metrics_registry.on(ToolCallResultEvent)
def _handle_better_edit_experiment(event: ToolCallResultEvent) -> None:
    """BetterEdit 工具实验审计：仅处理 BetterEdit，按 model:BetterEdit 聚合。"""
    if event.tool_name != "BetterEdit":
        return

    if not event.model:
        log.warning("BetterEdit experiment handler: event has no model, skipping")
        return

    key = f"{event.model}:BetterEdit"

    path = get_wing_home() / "metrics_experimental.json"
    data = _read_metrics_json(path)
    metrics = BetterEditMetrics.from_raw(data)

    entry = metrics.entries.get(key)
    if entry is None:
        metrics.entries[key] = BetterEditMetricsEntry().aggregate(event)
    else:
        entry.aggregate(event)

    data["tool_calls"] = metrics.to_raw()
    _atomic_write_json(path, data)
