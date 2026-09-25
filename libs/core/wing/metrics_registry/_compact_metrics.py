"""wing.metrics_registry._compact_metrics — Compact 审计 handler。

schema: CompactDetail, CompactMetrics (BaseModel)
handler: _handle_compact_global, _handle_compact_session
"""

from __future__ import annotations

from datetime import datetime

from pydantic import BaseModel, Field

from wing.common.logger import log
from wing.common.utils import _is_safe_path_component
from wing.config import get_config, get_wing_home
from wing.event import CompactDoneEvent
from wing.metrics_registry.core import (
    _atomic_write_json,
    _read_metrics_json,
    metrics_registry,
)

# ============================================================
# Schema — Compact 审计 entry
# ============================================================


class CompactDetail(BaseModel):
    """单次 compact 详情。"""

    original_tokens: int
    compressed_tokens: int
    model: str


class CompactMetrics(BaseModel):
    """Session 级 compact 审计容器。"""

    times: int = 0
    details: list[CompactDetail] = Field(default_factory=list)

    def append(self, event: CompactDoneEvent) -> CompactMetrics:
        """追加一次 compact 详情。返回 self（就地修改）。"""
        self.times += 1
        self.details.append(
            CompactDetail(
                original_tokens=event.original_tokens,
                compressed_tokens=event.compressed_tokens,
                model=event.model,
            )
        )
        return self

    @classmethod
    def from_raw(cls, data: dict) -> CompactMetrics:
        raw = data.get("compact")
        if raw is None:
            return cls()
        return cls.model_validate(raw)

    def to_raw(self) -> dict:
        return self.model_dump()


class GlobalCompactMetrics(BaseModel):
    """全局 compact 审计容器。key = yyyy-mm-dd → list[CompactDetail]。"""

    entries: dict[str, list[CompactDetail]] = Field(default_factory=dict)

    @classmethod
    def from_raw(cls, data: dict) -> GlobalCompactMetrics:
        raw = data.get("compact", {})
        entries = {}
        for key, val_list in raw.items():
            entries[key] = [CompactDetail.model_validate(v) for v in val_list]
        return cls(entries=entries)

    def to_raw(self) -> dict:
        return {k: [d.model_dump() for d in v] for k, v in self.entries.items()}


# ============================================================
# Handlers
# ============================================================


@metrics_registry.on(CompactDoneEvent)
def _handle_compact_global(event: CompactDoneEvent) -> None:
    """全局 compact 审计：按 yyyy-mm-dd → list[CompactDetail] 追加。"""
    if not event.model:
        log.warning(
            f"Compact global handler: event has no model (session={event.session_id}), skipping"
        )
        return

    date_str = datetime.now().strftime("%Y-%m-%d")

    path = get_wing_home() / "metrics.json"
    data = _read_metrics_json(path)
    metrics = GlobalCompactMetrics.from_raw(data)

    detail = CompactDetail(
        original_tokens=event.original_tokens,
        compressed_tokens=event.compressed_tokens,
        model=event.model,
    )
    metrics.entries.setdefault(date_str, []).append(detail)

    data["compact"] = metrics.to_raw()
    _atomic_write_json(path, data)


@metrics_registry.on(CompactDoneEvent)
def _handle_compact_session(event: CompactDoneEvent) -> None:
    """Session 级 compact 审计：追加 detail 到 CompactMetrics。"""
    if not event.model:
        log.warning(
            f"Compact session handler: event has no model (session={event.session_id}), skipping"
        )
        return
    if not event.session_id:
        log.warning("Compact session handler: event has no session_id, skipping")
        return
    if not _is_safe_path_component(event.session_id):
        log.warning(
            f"Compact session handler: unsafe session_id '{event.session_id}', skipping"
        )
        return

    sessions_path = get_config().sessions.resolved_path()
    path = sessions_path / event.session_id / "metrics.json"

    data = _read_metrics_json(path)
    metrics = CompactMetrics.from_raw(data)
    metrics.append(event)

    data["compact"] = metrics.to_raw()
    _atomic_write_json(path, data)
