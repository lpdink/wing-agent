"""wing.metrics_registry._llm_metrics — LLM 调用指标审计 handler。

schema: LLMCallMetricsEntry (BaseModel)
handler: _handle_global_metrics, _handle_session_metrics
"""

from __future__ import annotations

from datetime import datetime

from pydantic import BaseModel

from wing.common.logger import log
from wing.common.utils import _is_safe_path_component
from wing.config import get_config, get_wing_home
from wing.event import LLMCallMetricsEvent
from wing.metrics_registry.core import (
    _atomic_write_json,
    _read_metrics_json,
    metrics_registry,
)

# ============================================================
# Schema — LLM 调用指标 entry
# ============================================================


class LLMCallMetricsEntry(BaseModel):
    """单次聚合 entry：token 类累加，性能类存 total+count。"""

    prompt_tokens: int = 0
    completion_tokens: int = 0
    cached_tokens: int = 0
    first_chunk_rt_ms_total: float = 0
    first_chunk_rt_ms_count: int = 0
    tokens_per_sec_total: float = 0
    tokens_per_sec_count: int = 0

    def aggregate(self, event: LLMCallMetricsEvent) -> LLMCallMetricsEntry:
        """累加一次 LLM call 指标。返回 self（就地修改）。"""
        self.prompt_tokens += event.prompt_tokens
        self.completion_tokens += event.completion_tokens
        self.cached_tokens += event.cached_tokens
        self.first_chunk_rt_ms_total += event.first_chunk_rt_ms
        self.first_chunk_rt_ms_count += 1
        self.tokens_per_sec_total += event.tokens_per_sec
        self.tokens_per_sec_count += 1
        return self


class GlobalLLMMetrics(BaseModel):
    """全局 LLM metrics 容器。key = yyyy-mm-dd:model_name。"""

    entries: dict[str, LLMCallMetricsEntry] = {}

    @classmethod
    def from_raw(cls, data: dict) -> GlobalLLMMetrics:
        """从 metrics.json raw dict 构建。"""
        raw = data.get("llm_call_metrics", {})
        entries = {
            key: LLMCallMetricsEntry.model_validate(val) for key, val in raw.items()
        }
        return cls(entries=entries)

    def to_raw(self) -> dict:
        """导出为 metrics.json 的 llm_call_metrics section dict。"""
        return {k: v.model_dump() for k, v in self.entries.items()}


class SessionLLMMetrics(BaseModel):
    """Session 级 LLM metrics 容器。key = model_name。"""

    entries: dict[str, LLMCallMetricsEntry] = {}

    @classmethod
    def from_raw(cls, data: dict) -> SessionLLMMetrics:
        """从 metrics.json raw dict 构建。"""
        raw = data.get("llm_call_metrics", {})
        entries = {
            key: LLMCallMetricsEntry.model_validate(val) for key, val in raw.items()
        }
        return cls(entries=entries)

    def to_raw(self) -> dict:
        """导出为 metrics.json 的 llm_call_metrics section dict。"""
        return {k: v.model_dump() for k, v in self.entries.items()}


# ============================================================
# Handlers
# ============================================================


@metrics_registry.on(LLMCallMetricsEvent)
def _handle_global_metrics(event: LLMCallMetricsEvent) -> None:
    """全局 LLM 调用指标审计：按天+模型聚合到 WING_HOME/metrics.json。"""
    if not event.model:
        log.warning(
            f"Global metrics handler: event has no model (session={event.session_id}), skipping"
        )
        return

    date_str = datetime.now().strftime("%Y-%m-%d")
    key = f"{date_str}:{event.model}"

    path = get_wing_home() / "metrics.json"
    data = _read_metrics_json(path)
    metrics = GlobalLLMMetrics.from_raw(data)

    entry = metrics.entries.get(key)
    if entry is None:
        metrics.entries[key] = LLMCallMetricsEntry().aggregate(event)
    else:
        entry.aggregate(event)

    data["llm_call_metrics"] = metrics.to_raw()
    _atomic_write_json(path, data)


@metrics_registry.on(LLMCallMetricsEvent)
def _handle_session_metrics(event: LLMCallMetricsEvent) -> None:
    """Session 级别 LLM 调用指标审计：按模型聚合到 sessions/{sid}/metrics.json。"""
    if not event.model:
        log.warning(
            f"Session metrics handler: event has no model (session={event.session_id}), skipping"
        )
        return
    if not event.session_id:
        log.warning("Session metrics handler: event has no session_id, skipping")
        return
    if not _is_safe_path_component(event.session_id):
        log.warning(
            f"Session metrics handler: unsafe session_id '{event.session_id}', skipping"
        )
        return

    key = event.model
    sessions_path = get_config().sessions.resolved_path()
    path = sessions_path / event.session_id / "metrics.json"

    data = _read_metrics_json(path)
    metrics = SessionLLMMetrics.from_raw(data)

    entry = metrics.entries.get(key)
    if entry is None:
        metrics.entries[key] = LLMCallMetricsEntry().aggregate(event)
    else:
        entry.aggregate(event)

    data["llm_call_metrics"] = metrics.to_raw()
    _atomic_write_json(path, data)
