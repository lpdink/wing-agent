"""wing.metrics_registry._tool_call_metrics — 工具调用审计 handler。

schema: ToolCallMetricsEntry (BaseModel)
handler: _handle_tool_call_global, _handle_tool_call_session
"""

from __future__ import annotations

from datetime import datetime

from pydantic import BaseModel

from wing.common.logger import log
from wing.common.utils import _is_safe_path_component
from wing.config import get_config, get_wing_home
from wing.event import ToolCallResultEvent
from wing.metrics_registry.core import (
    _atomic_write_json,
    _read_metrics_json,
    metrics_registry,
)

# ============================================================
# Schema — 工具调用审计 entry
# ============================================================


class ToolCallMetricsEntry(BaseModel):
    """单次聚合 entry：调用次数、错误次数。"""

    times: int = 0
    error_times: int = 0

    def aggregate(self, event: ToolCallResultEvent) -> ToolCallMetricsEntry:
        """累加一次工具调用结果。返回 self（就地修改）。"""
        self.times += 1
        if not event.tool_success:
            self.error_times += 1
        return self


class GlobalToolCallMetrics(BaseModel):
    """全局工具调用审计容器。key = yyyy-mm-dd:model:tool_name。"""

    entries: dict[str, ToolCallMetricsEntry] = {}

    @classmethod
    def from_raw(cls, data: dict) -> GlobalToolCallMetrics:
        raw = data.get("tool_calls", {})
        entries = {}
        for key, val in raw.items():
            entries[key] = ToolCallMetricsEntry.model_validate(val)
        return cls(entries=entries)

    def to_raw(self) -> dict:
        return {k: v.model_dump() for k, v in self.entries.items()}


class SessionToolCallMetrics(BaseModel):
    """Session 级工具调用审计容器。key = model:tool_name。"""

    entries: dict[str, ToolCallMetricsEntry] = {}

    @classmethod
    def from_raw(cls, data: dict) -> SessionToolCallMetrics:
        raw = data.get("tool_calls", {})
        entries = {}
        for key, val in raw.items():
            entries[key] = ToolCallMetricsEntry.model_validate(val)
        return cls(entries=entries)

    def to_raw(self) -> dict:
        return {k: v.model_dump() for k, v in self.entries.items()}


# ============================================================
# Handlers
# ============================================================


# 全局工具调用审计。聚合键随工具种类增长；不需要时不注册本 handler 即可。
@metrics_registry.on(ToolCallResultEvent)
def _handle_tool_call_global(event: ToolCallResultEvent) -> None:
    """全局工具调用审计：按 yyyy-mm-dd:model:tool_name 聚合。"""
    if not event.model:
        log.warning(
            f"Tool call global handler: event has no model (tool={event.tool_name}), skipping"
        )
        return

    date_str = datetime.now().strftime("%Y-%m-%d")
    key = f"{date_str}:{event.model}:{event.tool_name}"

    path = get_wing_home() / "metrics.json"
    data = _read_metrics_json(path)
    metrics = GlobalToolCallMetrics.from_raw(data)

    entry = metrics.entries.get(key)
    if entry is None:
        metrics.entries[key] = ToolCallMetricsEntry().aggregate(event)
    else:
        entry.aggregate(event)

    data["tool_calls"] = metrics.to_raw()
    _atomic_write_json(path, data)


@metrics_registry.on(ToolCallResultEvent)
def _handle_tool_call_session(event: ToolCallResultEvent) -> None:
    """Session 级工具调用审计：按 model:tool_name 聚合。"""
    if not event.model:
        log.warning(
            f"Tool call session handler: event has no model (tool={event.tool_name}, session={event.session_id}), skipping"
        )
        return
    if not event.session_id:
        log.warning("Tool call session handler: event has no session_id, skipping")
        return
    if not _is_safe_path_component(event.session_id):
        log.warning(
            f"Tool call session handler: unsafe session_id '{event.session_id}', skipping"
        )
        return

    key = f"{event.model}:{event.tool_name}"
    sessions_path = get_config().sessions.resolved_path()
    path = sessions_path / event.session_id / "metrics.json"

    data = _read_metrics_json(path)
    metrics = SessionToolCallMetrics.from_raw(data)

    entry = metrics.entries.get(key)
    if entry is None:
        metrics.entries[key] = ToolCallMetricsEntry().aggregate(event)
    else:
        entry.aggregate(event)

    data["tool_calls"] = metrics.to_raw()
    _atomic_write_json(path, data)
