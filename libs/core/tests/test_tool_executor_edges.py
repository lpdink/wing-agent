"""ToolExecutor 边界：执行外壳兜底、VAR_KEYWORD 透传、截断开关、打断收尸。

主路径（并发、顺序、错误隔离）在 test_concurrent_tool_exec.py / test_tool_output_pipeline.py。
这里补的是「异常/极限」分支：未预期异常不得炸掉整轮、远程工具全量透传参数、
截断可关且落盘失败要降级、打断时收尸超时要强制收拢。
"""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

import pytest

from wing.agent.tool_executor import (
    INTERRUPTED_RESULT,
    InterruptedToolResults,
    ToolExecutor,
)
from wing.schema import Tool, ToolCall

pytestmark = pytest.mark.asyncio


class _Sink:
    """最小 AgentEventSink 替身：只记录工具起止。"""

    def __init__(self) -> None:
        self.finished: list[dict] = []

    def tool_started(self, tc: ToolCall) -> None:
        pass

    def tool_finished(
        self, tc, result, *, success, model, media=None, duration_ms=None
    ) -> None:
        self.finished.append(
            {"name": tc.name, "result": result, "success": success, "media": media}
        )


def _tool(fn: Any, name: str = "T") -> Tool:
    return Tool(name=name, namespace="default", description="", params=[], function=fn)


def _call(name: str = "T", **arguments: Any) -> ToolCall:
    return ToolCall(id="call-1", name=name, arguments=arguments)


# ── 执行外壳兜底 ──────────────────────────────────────────────


async def test_unexpected_error_becomes_result_not_exception(monkeypatch):
    """工具执行链上任何未预期异常都折成工具结果——整轮不能因它失败。"""
    executor = ToolExecutor(_Sink())  # ty: ignore[invalid-argument-type]

    async def tool() -> str:
        return "never runs"

    executor.set_tools({"T": _tool(tool)})

    def _boom(result: str) -> str:
        raise RuntimeError("truncation exploded")

    monkeypatch.setattr("wing.agent.tool_executor._maybe_truncate", _boom)

    results = await executor.execute([_call()], "m")

    assert results[0].role == "tool"
    assert results[0].content == "Error executing tool 'T': truncation exploded"


async def test_var_keyword_tool_receives_arguments_verbatim():
    """远程工具（**kwargs）不做参数裁剪：schema 在宿主侧，核心不知道参数名。"""
    executor = ToolExecutor(_Sink())  # ty: ignore[invalid-argument-type]
    seen: list[dict] = []

    async def remote(**kwargs: Any) -> str:
        seen.append(kwargs)
        return "ok"

    executor.set_tools({"T": _tool(remote)})

    await executor.execute([_call(cmd="ls", extra="kept")], "m")

    assert seen == [{"cmd": "ls", "extra": "kept"}]


async def test_unknown_arguments_are_filtered_for_normal_tools():
    executor = ToolExecutor(_Sink())  # ty: ignore[invalid-argument-type]
    seen: list[dict] = []

    async def tool(cmd: str) -> str:
        seen.append({"cmd": cmd})
        return "ok"

    executor.set_tools({"T": _tool(tool)})

    await executor.execute([_call(cmd="ls", hallucinated="boom")], "m")

    assert seen == [{"cmd": "ls"}]


# ── 截断开关 ──────────────────────────────────────────────────


async def test_truncation_disabled_keeps_full_result(_isolate_config):
    from wing.agent.tool_executor import _maybe_truncate

    _isolate_config.tool_result_truncate.max_length = None
    payload = "X" * 500

    assert _maybe_truncate(payload) == payload

    _isolate_config.tool_result_truncate.max_length = -1
    assert _maybe_truncate(payload) == payload


async def test_truncation_saves_full_result_to_wing_home(
    _isolate_config, tmp_path, monkeypatch
):
    from wing.agent.tool_executor import _maybe_truncate

    monkeypatch.setenv("WING_HOME", str(tmp_path / "wing-home"))
    _isolate_config.tool_result_truncate.max_length = 50
    _isolate_config.tool_result_truncate.keep_chars = 5

    truncated = _maybe_truncate("HEAD" + "X" * 500 + "TAIL")

    assert "truncated" in truncated
    saved = list((tmp_path / "wing-home" / "core" / "tmp").glob("wing_truncated_*.txt"))
    assert len(saved) == 1
    assert saved[0].read_text(encoding="utf-8").startswith("HEAD")


async def test_truncation_survives_unwritable_temp(_isolate_config, monkeypatch):
    """落盘失败只降级为提示：截断后的结果照常回给模型。"""
    import wing.agent.tool_executor as executor_mod

    monkeypatch.setenv("WING_HOME", "/nonexistent-wing-home")
    _isolate_config.tool_result_truncate.max_length = 50
    _isolate_config.tool_result_truncate.keep_chars = 5

    def _boom(*args: Any, **kwargs: Any):
        raise OSError("read-only file system")

    monkeypatch.setattr(executor_mod.tempfile, "NamedTemporaryFile", _boom)

    truncated = executor_mod._maybe_truncate("X" * 500)

    assert "could not be saved to disk" in truncated


# ── 打断收尸 ──────────────────────────────────────────────────


async def test_interrupt_gather_timeout_forces_synthesis(monkeypatch):
    """收尸窗口耗尽 → 强制为每个 call 合成 interrupted 结果（不留悬空 call）。

    「工具吞掉取消」在进程内无法稳定复现（asyncio 会合并重复取消请求），
    所以直接让收尸期的 gather 永不完成——这正是超时分支的存在理由。
    """
    import wing.agent.tool_executor as executor_mod

    monkeypatch.setattr(executor_mod, "_INTERRUPT_GATHER_TIMEOUT", 0.05)
    executor = ToolExecutor(_Sink())  # ty: ignore[invalid-argument-type]
    started = asyncio.Event()
    real_gather = asyncio.gather
    calls = {"n": 0}

    def _gather(*args: Any, **kwargs: Any):
        calls["n"] += 1
        if calls["n"] == 1:
            return real_gather(*args, **kwargs)
        return asyncio.get_running_loop().create_future()  # 永不完成

    async def hanging() -> str:
        started.set()
        await asyncio.sleep(30)
        return "never"

    executor.set_tools({"T": _tool(hanging)})
    monkeypatch.setattr(executor_mod.asyncio, "gather", _gather)

    task = asyncio.create_task(executor.execute([_call()], "m"))
    await started.wait()
    task.cancel()

    with pytest.raises(InterruptedToolResults) as excinfo:
        await task

    results = excinfo.value.results
    assert [m.content for m in results] == [INTERRUPTED_RESULT]
    assert results[0].tool_call_id == "call-1"


async def test_interrupt_keeps_real_result_when_tool_finished(monkeypatch):
    """收尸窗口内正常完成的工具保留真实结果，不合成。"""
    monkeypatch.setattr("wing.agent.tool_executor._INTERRUPT_GATHER_TIMEOUT", 5.0)
    executor = ToolExecutor(_Sink())  # ty: ignore[invalid-argument-type]
    started = asyncio.Event()

    async def quick_child() -> str:
        started.set()
        try:
            await asyncio.sleep(30)
        except asyncio.CancelledError:
            return "cancelled-but-done"
        return "never"  # pragma: no cover - 收尸窗口内被取消，走上面那条

    executor.set_tools({"T": _tool(quick_child)})

    task = asyncio.create_task(executor.execute([_call()], "m"))
    await started.wait()
    task.cancel()

    with pytest.raises(InterruptedToolResults) as excinfo:
        await task

    assert [m.content for m in excinfo.value.results] == ["cancelled-but-done"]


async def test_interrupted_results_keep_argument_error_precheck():
    """打断与本轮无关节点的组合：参数非法仍走短路，不执行工具。"""
    executor = ToolExecutor(_Sink())  # ty: ignore[invalid-argument-type]
    called = False

    async def tool(**kwargs: Any) -> str:
        nonlocal called
        called = True
        return "ok"

    executor.set_tools({"T": _tool(tool)})
    tc = ToolCall(id="c", name="T", arguments={}, arguments_error="bad json")

    results = await executor.execute([tc], "m")

    assert called is False
    assert "was NOT executed" in (results[0].content or "")


async def test_media_refs_pass_through_message(tmp_path: Path):
    """工具返回 media → 消息携带引用、内容截断只作用于文本。"""
    from wing.schema import MediaRef, ToolOutput

    executor = ToolExecutor(_Sink())  # ty: ignore[invalid-argument-type]
    ref = MediaRef(id="a" * 64, mime="image/png", bytes=1, width=1, height=1)

    async def image_tool() -> ToolOutput:
        return ToolOutput(content="img", media=[ref])

    executor.set_tools({"T": _tool(image_tool)})

    results = await executor.execute([_call()], "m")

    assert results[0].media == [ref]
    assert results[0].content == "img"
