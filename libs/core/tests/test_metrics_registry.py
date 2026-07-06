"""
tests/test_metrics_registry.py — MetricsRegistry 单元测试

覆盖：
  - MetricsRegistry 注册与分发机制
  - LLMCallMetricsEntry (BaseModel) 反序列化与聚合
  - ToolCallMetricsEntry (BaseModel) 反序列化与聚合
  - CompactDetail / CompactMetrics (BaseModel) 反序列化与追加
  - 全局 / Session 级 handler 持久化
  - 路径安全校验
"""

import json
import tempfile
from datetime import datetime
from pathlib import Path
from unittest import mock

import pytest

from wing.event import (
    CompactDoneEvent,
    LLMCallMetricsEvent,
    WingEvent,
    TextEvent,
    ToolCallResultEvent,
)
from wing.metrics_registry import MetricsRegistry
from wing.metrics_registry._llm_metrics import (
    LLMCallMetricsEntry,
    GlobalLLMMetrics,
    SessionLLMMetrics,
    _handle_global_metrics,
    _handle_session_metrics,
)
from wing.metrics_registry._tool_call_metrics import (
    ToolCallMetricsEntry,
    _handle_tool_call_global,
    _handle_tool_call_session,
)
from wing.metrics_registry._compact_metrics import (
    CompactDetail,
    CompactMetrics,
    _handle_compact_global,
    _handle_compact_session,
)
from wing.common.utils import _is_safe_path_component


# ============================================================
# Fixtures
# ============================================================


@pytest.fixture
def sample_llm_event():
    return LLMCallMetricsEvent(
        model="glm-5",
        prompt_tokens=100,
        completion_tokens=50,
        cached_tokens=20,
        first_chunk_rt_ms=1200.0,
        tokens_per_sec=45.0,
        session_id="test-session-1",
    )


@pytest.fixture
def sample_llm_event2():
    return LLMCallMetricsEvent(
        model="glm-5",
        prompt_tokens=200,
        completion_tokens=80,
        cached_tokens=50,
        first_chunk_rt_ms=800.0,
        tokens_per_sec=55.0,
        session_id="test-session-1",
    )


@pytest.fixture
def sample_llm_event_diff_model():
    return LLMCallMetricsEvent(
        model="qwen3.5-plus",
        prompt_tokens=300,
        completion_tokens=150,
        cached_tokens=100,
        first_chunk_rt_ms=1500.0,
        tokens_per_sec=35.0,
        session_id="test-session-1",
    )


@pytest.fixture
def sample_tool_call_success():
    return ToolCallResultEvent(
        model="glm-5",
        tool_name="Bash",
        tool_args={"command": "ls"},
        tool_call_id="tc-1",
        tool_result="file1 file2",
        tool_success=True,
        session_id="test-session-1",
    )


@pytest.fixture
def sample_tool_call_fail():
    return ToolCallResultEvent(
        model="glm-5",
        tool_name="Bash",
        tool_args={"command": "ls"},
        tool_call_id="tc-2",
        tool_result="Error: unknown tool: Bash",
        tool_success=False,
        session_id="test-session-1",
    )


@pytest.fixture
def sample_compact_event():
    return CompactDoneEvent(
        model="glm-5",
        original_tokens=5000,
        compressed_tokens=500,
        session_id="test-session-1",
    )


@pytest.fixture
def sample_compact_event2():
    return CompactDoneEvent(
        model="qwen3.5-plus",
        original_tokens=8000,
        compressed_tokens=800,
        session_id="test-session-1",
    )


# ============================================================
# LLMCallMetricsEntry BaseModel 测试
# ============================================================


class TestLLMCallMetricsEntry:
    """测试 LLMCallMetricsEntry BaseModel 反序列化与聚合"""

    def test_default_values(self):
        entry = LLMCallMetricsEntry()
        assert entry.prompt_tokens == 0
        assert entry.first_chunk_rt_ms_count == 0

    def test_model_validate_from_dict(self, sample_llm_event):
        """从 dict 反序列化"""
        entry = LLMCallMetricsEntry().aggregate(sample_llm_event)
        restored = LLMCallMetricsEntry.model_validate(entry.model_dump())
        assert restored.prompt_tokens == 100
        assert restored.completion_tokens == 50
        assert restored.first_chunk_rt_ms_count == 1

    def test_aggregate_accumulates(self, sample_llm_event, sample_llm_event2):
        """累加两次事件"""
        entry = LLMCallMetricsEntry().aggregate(sample_llm_event)
        entry.aggregate(sample_llm_event2)
        assert entry.prompt_tokens == 300
        assert entry.completion_tokens == 130
        assert entry.cached_tokens == 70
        assert entry.first_chunk_rt_ms_total == 2000.0
        assert entry.first_chunk_rt_ms_count == 2

    def test_model_validate_wrong_type_raises(self):
        """字段类型不匹配时 model_validate 报错"""
        with pytest.raises(Exception):
            LLMCallMetricsEntry.model_validate({"prompt_tokens": "not_a_number"})


# ============================================================
# ToolCallMetricsEntry BaseModel 测试
# ============================================================


class TestToolCallMetricsEntry:
    """测试 ToolCallMetricsEntry BaseModel 反序列化与聚合"""

    def test_default_values(self):
        entry = ToolCallMetricsEntry()
        assert entry.times == 0
        assert entry.error_times == 0

    def test_model_validate_from_dict(self):
        """从 dict 反序列化"""
        raw = {"times": 5, "error_times": 2}
        entry = ToolCallMetricsEntry.model_validate(raw)
        assert entry.times == 5
        assert entry.error_times == 2

    def test_aggregate_success(self, sample_tool_call_success):
        """成功调用：times+1，error 不变"""
        entry = ToolCallMetricsEntry().aggregate(sample_tool_call_success)
        assert entry.times == 1
        assert entry.error_times == 0

    def test_aggregate_failure(self, sample_tool_call_fail):
        """失败调用：times+1，error_times+1"""
        entry = ToolCallMetricsEntry().aggregate(sample_tool_call_fail)
        assert entry.times == 1
        assert entry.error_times == 1

    def test_aggregate_same_error_twice(self, sample_tool_call_fail):
        """同一错误两次：error_times 计数累加"""
        entry = ToolCallMetricsEntry().aggregate(sample_tool_call_fail)
        entry.aggregate(sample_tool_call_fail)
        assert entry.times == 2
        assert entry.error_times == 2

    def test_model_validate_wrong_type_raises(self):
        """字段类型不匹配时 model_validate 报错"""
        with pytest.raises(Exception):
            ToolCallMetricsEntry.model_validate({"times": "not_a_number"})


# ============================================================
# CompactDetail / CompactMetrics BaseModel 测试
# ============================================================


class TestCompactDetail:
    """测试 CompactDetail BaseModel"""

    def test_model_validate_from_dict(self):
        raw = {"original_tokens": 5000, "compressed_tokens": 500, "model": "glm-5"}
        detail = CompactDetail.model_validate(raw)
        assert detail.original_tokens == 5000
        assert detail.model == "glm-5"

    def test_model_validate_missing_field_raises(self):
        """缺少必填字段时报错"""
        with pytest.raises(Exception):
            CompactDetail.model_validate({"original_tokens": 5000})


class TestCompactMetrics:
    """测试 CompactMetrics BaseModel"""

    def test_default_values(self):
        m = CompactMetrics()
        assert m.times == 0
        assert m.details == []

    def test_from_raw_empty(self):
        m = CompactMetrics.from_raw({})
        assert m.times == 0

    def test_from_raw_existing(self):
        raw = {
            "compact": {
                "times": 2,
                "details": [
                    {
                        "original_tokens": 5000,
                        "compressed_tokens": 500,
                        "model": "glm-5",
                    },
                ],
            }
        }
        m = CompactMetrics.from_raw(raw)
        assert m.times == 2
        assert len(m.details) == 1
        assert m.details[0].model == "glm-5"

    def test_append(self, sample_compact_event):
        m = CompactMetrics()
        m.append(sample_compact_event)
        assert m.times == 1
        assert len(m.details) == 1
        assert m.details[0].original_tokens == 5000
        assert m.details[0].model == "glm-5"

    def test_append_twice(self, sample_compact_event, sample_compact_event2):
        m = CompactMetrics()
        m.append(sample_compact_event)
        m.append(sample_compact_event2)
        assert m.times == 2
        assert len(m.details) == 2
        assert m.details[1].model == "qwen3.5-plus"


# ============================================================
# GlobalLLMMetrics / SessionLLMMetrics 容器测试
# ============================================================


class TestGlobalLLMMetrics:
    """测试 GlobalLLMMetrics from_raw / to_raw"""

    def test_from_raw_empty(self):
        m = GlobalLLMMetrics.from_raw({})
        assert m.entries == {}

    def test_from_raw_with_entries(self, sample_llm_event):
        entry = LLMCallMetricsEntry().aggregate(sample_llm_event)
        m = GlobalLLMMetrics(entries={"2026-01-01:glm-5": entry})
        raw = m.to_raw()
        assert "2026-01-01:glm-5" in raw
        assert raw["2026-01-01:glm-5"]["prompt_tokens"] == 100

    def test_roundtrip(self, sample_llm_event):
        """from_raw → to_raw → from_raw 保持一致"""
        entry = LLMCallMetricsEntry().aggregate(sample_llm_event)
        m = GlobalLLMMetrics(entries={"k": entry})
        restored = GlobalLLMMetrics.from_raw({"llm_call_metrics": m.to_raw()})
        assert restored.entries["k"].prompt_tokens == 100


class TestSessionLLMMetrics:
    """测试 SessionLLMMetrics from_raw / to_raw"""

    def test_from_raw_empty(self):
        m = SessionLLMMetrics.from_raw({})
        assert m.entries == {}

    def test_roundtrip(self, sample_llm_event):
        entry = LLMCallMetricsEntry().aggregate(sample_llm_event)
        m = SessionLLMMetrics(entries={"glm-5": entry})
        restored = SessionLLMMetrics.from_raw({"llm_call_metrics": m.to_raw()})
        assert restored.entries["glm-5"].prompt_tokens == 100


# ============================================================
# MetricsRegistry 注册与分发
# ============================================================


class TestMetricsRegistry:
    """测试 MetricsRegistry 注册与分发"""

    def test_register_and_dispatch(self):
        registry = MetricsRegistry()
        calls = []

        def handler(event):
            calls.append(event.model)

        registry.register(LLMCallMetricsEvent, handler)
        event = LLMCallMetricsEvent(
            model="test-model",
            prompt_tokens=10,
            completion_tokens=5,
            cached_tokens=0,
            first_chunk_rt_ms=100.0,
            tokens_per_sec=10.0,
        )
        registry.handle(event)
        assert calls == ["test-model"]

    def test_multiple_handlers_same_event(self):
        registry = MetricsRegistry()
        order = []

        def handler_a(event):
            order.append("a")

        def handler_b(event):
            order.append("b")

        registry.register(LLMCallMetricsEvent, handler_a)
        registry.register(LLMCallMetricsEvent, handler_b)
        event = LLMCallMetricsEvent(
            model="m",
            prompt_tokens=0,
            completion_tokens=0,
            cached_tokens=0,
            first_chunk_rt_ms=0.0,
            tokens_per_sec=0.0,
        )
        registry.handle(event)
        assert order == ["a", "b"]

    def test_unrelated_event_not_dispatched(self):
        registry = MetricsRegistry()
        calls = []

        def handler(event):
            calls.append(1)

        registry.register(LLMCallMetricsEvent, handler)
        registry.handle(TextEvent(content="hello"))
        assert calls == []

    def test_handler_exception_does_not_break_chain(self):
        registry = MetricsRegistry()
        calls = []

        def failing_handler(event):
            raise ValueError("oops")

        def good_handler(event):
            calls.append("ok")

        registry.register(LLMCallMetricsEvent, failing_handler)
        registry.register(LLMCallMetricsEvent, good_handler)
        event = LLMCallMetricsEvent(
            model="m",
            prompt_tokens=0,
            completion_tokens=0,
            cached_tokens=0,
            first_chunk_rt_ms=0.0,
            tokens_per_sec=0.0,
        )
        registry.handle(event)
        assert calls == ["ok"]

    def test_handler_with_base_event_class(self):
        registry = MetricsRegistry()
        calls = []

        def handler(event):
            calls.append(type(event).__name__)

        registry.register(WingEvent, handler)
        registry.handle(TextEvent(content="hello"))
        registry.handle(
            LLMCallMetricsEvent(
                model="m",
                prompt_tokens=0,
                completion_tokens=0,
                cached_tokens=0,
                first_chunk_rt_ms=0.0,
                tokens_per_sec=0.0,
            )
        )
        assert "TextEvent" in calls
        assert "LLMCallMetricsEvent" in calls


# ============================================================
# LLM 全局 Handler 持久化测试
# ============================================================


class TestGlobalLLMHandler:
    """测试全局 LLM metrics.json 持久化"""

    @pytest.fixture
    def tmp_home(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            with mock.patch(
                "wing.metrics_registry._llm_metrics.get_wing_home",
                return_value=Path(tmpdir),
            ):
                yield Path(tmpdir)

    def _read_metrics(self, path: Path) -> dict:
        metrics_path = path / "metrics.json"
        if metrics_path.exists():
            return json.loads(metrics_path.read_text(encoding="utf-8"))
        return {}

    def test_first_write(self, tmp_home, sample_llm_event):
        _handle_global_metrics(sample_llm_event)
        data = self._read_metrics(tmp_home)
        key = f"{datetime.now().strftime('%Y-%m-%d')}:glm-5"
        assert key in data["llm_call_metrics"]
        entry = data["llm_call_metrics"][key]
        assert entry["prompt_tokens"] == 100
        assert entry["completion_tokens"] == 50
        assert entry["first_chunk_rt_ms_count"] == 1

    def test_aggregation_same_day_same_model(
        self, tmp_home, sample_llm_event, sample_llm_event2
    ):
        _handle_global_metrics(sample_llm_event)
        _handle_global_metrics(sample_llm_event2)
        data = self._read_metrics(tmp_home)
        key = f"{datetime.now().strftime('%Y-%m-%d')}:glm-5"
        entry = data["llm_call_metrics"][key]
        assert entry["prompt_tokens"] == 300
        assert entry["first_chunk_rt_ms_count"] == 2

    def test_cross_model_same_day(
        self, tmp_home, sample_llm_event, sample_llm_event_diff_model
    ):
        _handle_global_metrics(sample_llm_event)
        _handle_global_metrics(sample_llm_event_diff_model)
        data = self._read_metrics(tmp_home)
        date_str = datetime.now().strftime("%Y-%m-%d")
        assert data["llm_call_metrics"][f"{date_str}:glm-5"]["prompt_tokens"] == 100
        assert (
            data["llm_call_metrics"][f"{date_str}:qwen3.5-plus"]["prompt_tokens"] == 300
        )

    def test_no_model_skipped(self, tmp_home):
        event = LLMCallMetricsEvent(
            model="",
            prompt_tokens=100,
            completion_tokens=50,
            cached_tokens=0,
            first_chunk_rt_ms=100.0,
            tokens_per_sec=10.0,
        )
        _handle_global_metrics(event)
        data = self._read_metrics(tmp_home)
        assert data == {}


# ============================================================
# LLM Session Handler 持久化测试
# ============================================================


class TestSessionLLMHandler:
    """测试 session 级 LLM metrics.json 持久化"""

    @pytest.fixture
    def tmp_sessions(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            with mock.patch(
                "wing.metrics_registry._llm_metrics.get_config"
            ) as mock_get_config:
                mock_config = mock_get_config.return_value
                mock_config.sessions.resolved_path.return_value = Path(tmpdir)
                yield Path(tmpdir)

    def _read_metrics(self, session_dir: Path) -> dict:
        metrics_path = session_dir / "metrics.json"
        if metrics_path.exists():
            return json.loads(metrics_path.read_text(encoding="utf-8"))
        return {}

    def test_first_write(self, tmp_sessions, sample_llm_event):
        _handle_session_metrics(sample_llm_event)
        session_dir = tmp_sessions / "test-session-1"
        data = self._read_metrics(session_dir)
        assert data["llm_call_metrics"]["glm-5"]["prompt_tokens"] == 100

    def test_aggregation_same_model(
        self, tmp_sessions, sample_llm_event, sample_llm_event2
    ):
        _handle_session_metrics(sample_llm_event)
        _handle_session_metrics(sample_llm_event2)
        session_dir = tmp_sessions / "test-session-1"
        data = self._read_metrics(session_dir)
        assert data["llm_call_metrics"]["glm-5"]["prompt_tokens"] == 300

    def test_cross_model(
        self, tmp_sessions, sample_llm_event, sample_llm_event_diff_model
    ):
        _handle_session_metrics(sample_llm_event)
        _handle_session_metrics(sample_llm_event_diff_model)
        session_dir = tmp_sessions / "test-session-1"
        data = self._read_metrics(session_dir)
        assert data["llm_call_metrics"]["glm-5"]["prompt_tokens"] == 100
        assert data["llm_call_metrics"]["qwen3.5-plus"]["prompt_tokens"] == 300

    def test_no_session_id_skipped(self, tmp_sessions):
        event = LLMCallMetricsEvent(
            model="glm-5",
            prompt_tokens=100,
            completion_tokens=50,
            cached_tokens=0,
            first_chunk_rt_ms=100.0,
            tokens_per_sec=10.0,
            session_id=None,
        )
        _handle_session_metrics(event)
        assert not any(tmp_sessions.iterdir())

    def test_no_model_skipped(self, tmp_sessions):
        event = LLMCallMetricsEvent(
            model="",
            prompt_tokens=100,
            completion_tokens=50,
            cached_tokens=0,
            first_chunk_rt_ms=100.0,
            tokens_per_sec=10.0,
            session_id="test-session-1",
        )
        _handle_session_metrics(event)
        assert not (tmp_sessions / "test-session-1").exists()


# ============================================================
# ToolCall Global Handler 持久化测试
# ============================================================


class TestGlobalToolCallHandler:
    """测试全局工具调用 metrics.json 持久化"""

    @pytest.fixture
    def tmp_home(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            with mock.patch(
                "wing.metrics_registry._tool_call_metrics.get_wing_home",
                return_value=Path(tmpdir),
            ):
                yield Path(tmpdir)

    def _read_metrics(self, path: Path) -> dict:
        metrics_path = path / "metrics.json"
        if metrics_path.exists():
            return json.loads(metrics_path.read_text(encoding="utf-8"))
        return {}

    def test_first_success_write(self, tmp_home, sample_tool_call_success):
        _handle_tool_call_global(sample_tool_call_success)
        data = self._read_metrics(tmp_home)
        key = f"{datetime.now().strftime('%Y-%m-%d')}:glm-5:Bash"
        assert key in data["tool_calls"]
        entry = data["tool_calls"][key]
        assert entry["times"] == 1
        assert entry["error_times"] == 0

    def test_failure(self, tmp_home, sample_tool_call_fail):
        _handle_tool_call_global(sample_tool_call_fail)
        data = self._read_metrics(tmp_home)
        key = f"{datetime.now().strftime('%Y-%m-%d')}:glm-5:Bash"
        entry = data["tool_calls"][key]
        assert entry["times"] == 1
        assert entry["error_times"] == 1

    def test_cross_day_bucketing(self, tmp_home, sample_tool_call_success):
        """不同日期分桶"""
        today = datetime.now().strftime("%Y-%m-%d")
        _handle_tool_call_global(sample_tool_call_success)
        data = self._read_metrics(tmp_home)
        assert f"{today}:glm-5:Bash" in data["tool_calls"]

    def test_no_model_skipped(self, tmp_home):
        event = ToolCallResultEvent(
            model="",
            tool_name="Bash",
            tool_args={},
            tool_call_id="x",
            tool_result="ok",
            tool_success=True,
        )
        _handle_tool_call_global(event)
        data = self._read_metrics(tmp_home)
        assert data == {}


# ============================================================
# ToolCall Session Handler 持久化测试
# ============================================================


class TestSessionToolCallHandler:
    """测试 session 级工具调用 metrics.json 持久化"""

    @pytest.fixture
    def tmp_sessions(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            with mock.patch(
                "wing.metrics_registry._tool_call_metrics.get_config"
            ) as mock_get_config:
                mock_config = mock_get_config.return_value
                mock_config.sessions.resolved_path.return_value = Path(tmpdir)
                yield Path(tmpdir)

    def _read_metrics(self, session_dir: Path) -> dict:
        metrics_path = session_dir / "metrics.json"
        if metrics_path.exists():
            return json.loads(metrics_path.read_text(encoding="utf-8"))
        return {}

    def test_success(self, tmp_sessions, sample_tool_call_success):
        _handle_tool_call_session(sample_tool_call_success)
        session_dir = tmp_sessions / "test-session-1"
        data = self._read_metrics(session_dir)
        entry = data["tool_calls"]["glm-5:Bash"]
        assert entry["times"] == 1
        assert entry["error_times"] == 0

    def test_failure(self, tmp_sessions, sample_tool_call_fail):
        _handle_tool_call_session(sample_tool_call_fail)
        session_dir = tmp_sessions / "test-session-1"
        data = self._read_metrics(session_dir)
        entry = data["tool_calls"]["glm-5:Bash"]
        assert entry["times"] == 1
        assert entry["error_times"] == 1

    def test_failure_twice(self, tmp_sessions, sample_tool_call_fail):
        """失败两次：error_times 计数累加"""
        _handle_tool_call_session(sample_tool_call_fail)
        _handle_tool_call_session(sample_tool_call_fail)
        session_dir = tmp_sessions / "test-session-1"
        data = self._read_metrics(session_dir)
        entry = data["tool_calls"]["glm-5:Bash"]
        assert entry["times"] == 2
        assert entry["error_times"] == 2

    def test_no_model_skipped(self, tmp_sessions):
        event = ToolCallResultEvent(
            model="",
            tool_name="Bash",
            tool_args={},
            tool_call_id="x",
            tool_result="ok",
            tool_success=True,
            session_id="test-session-1",
        )
        _handle_tool_call_session(event)
        assert not (tmp_sessions / "test-session-1").exists()

    def test_no_session_id_skipped(self, tmp_sessions):
        event = ToolCallResultEvent(
            model="glm-5",
            tool_name="Bash",
            tool_args={},
            tool_call_id="x",
            tool_result="ok",
            tool_success=True,
            session_id=None,
        )
        _handle_tool_call_session(event)
        assert not any(tmp_sessions.iterdir())


# ============================================================
# Compact Global Handler 持久化测试
# ============================================================


class TestGlobalCompactHandler:
    """测试全局 compact metrics.json 持久化"""

    @pytest.fixture
    def tmp_home(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            with mock.patch(
                "wing.metrics_registry._compact_metrics.get_wing_home",
                return_value=Path(tmpdir),
            ):
                yield Path(tmpdir)

    def _read_metrics(self, path: Path) -> dict:
        metrics_path = path / "metrics.json"
        if metrics_path.exists():
            return json.loads(metrics_path.read_text(encoding="utf-8"))
        return {}

    def test_first_compact(self, tmp_home, sample_compact_event):
        _handle_compact_global(sample_compact_event)
        data = self._read_metrics(tmp_home)
        date_str = datetime.now().strftime("%Y-%m-%d")
        assert date_str in data["compact"]
        detail = data["compact"][date_str][0]
        assert detail["original_tokens"] == 5000
        assert detail["compressed_tokens"] == 500
        assert detail["model"] == "glm-5"

    def test_append_same_day(
        self, tmp_home, sample_compact_event, sample_compact_event2
    ):
        """同日追加多条"""
        _handle_compact_global(sample_compact_event)
        _handle_compact_global(sample_compact_event2)
        data = self._read_metrics(tmp_home)
        date_str = datetime.now().strftime("%Y-%m-%d")
        assert len(data["compact"][date_str]) == 2

    def test_no_model_skipped(self, tmp_home):
        event = CompactDoneEvent(
            model="",
            original_tokens=1000,
            compressed_tokens=100,
        )
        _handle_compact_global(event)
        data = self._read_metrics(tmp_home)
        assert data == {}


# ============================================================
# Compact Session Handler 持久化测试
# ============================================================


class TestSessionCompactHandler:
    """测试 session 级 compact metrics.json 持久化"""

    @pytest.fixture
    def tmp_sessions(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            with mock.patch(
                "wing.metrics_registry._compact_metrics.get_config"
            ) as mock_get_config:
                mock_config = mock_get_config.return_value
                mock_config.sessions.resolved_path.return_value = Path(tmpdir)
                yield Path(tmpdir)

    def _read_metrics(self, session_dir: Path) -> dict:
        metrics_path = session_dir / "metrics.json"
        if metrics_path.exists():
            return json.loads(metrics_path.read_text(encoding="utf-8"))
        return {}

    def test_first_compact(self, tmp_sessions, sample_compact_event):
        _handle_compact_session(sample_compact_event)
        session_dir = tmp_sessions / "test-session-1"
        data = self._read_metrics(session_dir)
        assert data["compact"]["times"] == 1
        assert len(data["compact"]["details"]) == 1
        assert data["compact"]["details"][0]["original_tokens"] == 5000
        assert data["compact"]["details"][0]["model"] == "glm-5"

    def test_append_twice(
        self, tmp_sessions, sample_compact_event, sample_compact_event2
    ):
        _handle_compact_session(sample_compact_event)
        _handle_compact_session(sample_compact_event2)
        session_dir = tmp_sessions / "test-session-1"
        data = self._read_metrics(session_dir)
        assert data["compact"]["times"] == 2
        assert len(data["compact"]["details"]) == 2
        assert data["compact"]["details"][1]["model"] == "qwen3.5-plus"

    def test_no_model_skipped(self, tmp_sessions):
        event = CompactDoneEvent(
            model="",
            original_tokens=1000,
            compressed_tokens=100,
            session_id="test-session-1",
        )
        _handle_compact_session(event)
        assert not (tmp_sessions / "test-session-1").exists()

    def test_no_session_id_skipped(self, tmp_sessions):
        event = CompactDoneEvent(
            model="glm-5",
            original_tokens=1000,
            compressed_tokens=100,
            session_id=None,
        )
        _handle_compact_session(event)
        assert not any(tmp_sessions.iterdir())


# ============================================================
# 路径安全校验
# ============================================================


class TestSafePathComponent:
    """测试路径安全校验"""

    def test_normal_session_id(self):
        assert _is_safe_path_component("20260521-abcd1234") is True

    def test_empty_string(self):
        assert _is_safe_path_component("") is False

    def test_dotdot(self):
        assert _is_safe_path_component("..") is False

    def test_forward_slash(self):
        assert _is_safe_path_component("foo/bar") is False

    def test_backslash(self):
        assert _is_safe_path_component("foo\\bar") is False


class TestPathTraversal:
    """测试各 handler 拒绝路径穿越 session_id"""

    @pytest.fixture
    def tmp_sessions(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            with mock.patch(
                "wing.metrics_registry._llm_metrics.get_config"
            ) as mock_get_config:
                mock_config = mock_get_config.return_value
                mock_config.sessions.resolved_path.return_value = Path(tmpdir)
                yield Path(tmpdir)

    def test_dotdot_session_id_skipped(self, tmp_sessions):
        event = LLMCallMetricsEvent(
            model="glm-5",
            prompt_tokens=100,
            completion_tokens=50,
            cached_tokens=0,
            first_chunk_rt_ms=100.0,
            tokens_per_sec=10.0,
            session_id="../etc",
        )
        _handle_session_metrics(event)
        assert not any(tmp_sessions.iterdir())

    def test_slash_session_id_skipped(self, tmp_sessions):
        event = LLMCallMetricsEvent(
            model="glm-5",
            prompt_tokens=100,
            completion_tokens=50,
            cached_tokens=0,
            first_chunk_rt_ms=100.0,
            tokens_per_sec=10.0,
            session_id="foo/bar",
        )
        _handle_session_metrics(event)
        assert not any(tmp_sessions.iterdir())
