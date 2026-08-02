# wing/agent/tool_executor.py
"""ToolExecutor — 工具并发执行 + 中断收尸。

职责：
- 并发调度多个 tool call（asyncio.gather）
- 中断时收拢每个 call 的最终结果（真实或合成），保证上下文不悬空
- 单工具执行：hook → 参数过滤 → 调用 → 截断
- 事件通过 AgentEventSink 发射
"""

from __future__ import annotations

import asyncio
import inspect
import tempfile
from contextvars import ContextVar
from pathlib import Path
from typing import Any

from wing.common.logger import log
from wing.config import get_config, get_wing_home
from wing.hook_registry import hooks
from wing.schema import Message, ToolCall, ToolError

from .event_sink import AgentEventSink

# 当前正在执行的工具调用 id。execute() 为每个工具任务设置（gather 各 task
# 的 context 相互隔离），ask_feedback() 据此把 AskEvent 与 feedback waiter 关联。
_current_tool_call_id: ContextVar[str | None] = ContextVar(
    "current_tool_call_id", default=None
)

# 被打断的未完成工具调用写入的统一结果内容。
INTERRUPTED_RESULT = "Tool call interrupted by user."

# 取消处理路径中等待工具 task 收尸的超时（秒）。
_INTERRUPT_GATHER_TIMEOUT = 5.0


def current_tool_call_id() -> str | None:
    """当前执行上下文的工具调用 id（不在工具执行中时为 None）。

    工具据此把派生事件（如 DiffContentEvent）关联回自身的 ToolCall，
    使前端在并发乱序场景下仍能把事件锚定到正确的 ToolCall cell。
    """
    return _current_tool_call_id.get()


class InterruptedToolResults(Exception):
    """execute() 被中断：携带每个 call 的最终结果（真实或合成）。

    worker 被取消时，execute() 不让 CancelledError 直接逃逸——那样本轮
    工具结果全部丢失，assistant 消息的 tool_calls 将悬空。它收拢每个
    call 的最终结果并向调用方抛此异常：调用方沿正常路径提交本轮消息，
    然后再重新抛出原始 CancelledError 让 worker 终止。
    """

    def __init__(
        self, results: list[Message], original: asyncio.CancelledError
    ) -> None:
        self.results = results
        self.original = original
        super().__init__("tool execution interrupted")


class ToolExecutor:
    """工具并发执行器。"""

    def __init__(self, sink: AgentEventSink) -> None:
        self._sink = sink
        # 工具表由外部设置（set_tools 时更新）
        self._tools: dict[str, Any] = {}

    def set_tools(self, tools: dict[str, Any]) -> None:
        self._tools = tools

    async def execute(
        self, pending_tool_calls: list[ToolCall], model: str
    ) -> list[Message]:
        """并发执行所有 tool calls，返回对应的 tool 消息列表。

        model 由调用点传入（ReActLoop 从唯一存储取值）——ToolExecutor 不持有
        model，仅用于 tool_finished 事件上报。中断时抛 InterruptedToolResults
        （携带真实/合成结果）。
        """
        log.info(f"exec_tool_calls: executing {len(pending_tool_calls)} tool calls")

        async def _exec_one(tc: ToolCall) -> Message:
            log.info(f"exec_tool_calls: executing tool '{tc.name}' with id={tc.id}")
            token = _current_tool_call_id.set(tc.id)
            try:
                result = await self._execute_one(tc, model)
                result = _maybe_truncate(result)
            except Exception as e:
                log.error(f"exec_tool_calls: unexpected error in tool '{tc.name}': {e}")
                result = f"Error executing tool '{tc.name}': {e}"
            finally:
                _current_tool_call_id.reset(token)
            return Message(role="tool", tool_call_id=tc.id, content=result)

        tasks = [asyncio.create_task(_exec_one(tc)) for tc in pending_tool_calls]
        try:
            return list(await asyncio.gather(*tasks))
        except asyncio.CancelledError as cancel_err:
            for task in tasks:
                task.cancel()
            try:
                raw = await asyncio.wait_for(
                    asyncio.gather(*tasks, return_exceptions=True),
                    timeout=_INTERRUPT_GATHER_TIMEOUT,
                )
            except asyncio.TimeoutError:
                log.warning(
                    "exec_tool_calls: gather timed out during interrupt cleanup; "
                    "forcing remaining tasks"
                )
                raw = []
                for task in tasks:
                    task.cancel()
                    if (
                        task.done()
                        and not task.cancelled()
                        and task.exception() is None
                    ):
                        raw.append(task.result())
                    else:
                        raw.append(None)

            results: list[Message] = []
            for tc, item in zip(pending_tool_calls, raw, strict=True):
                if isinstance(item, Message):
                    results.append(item)
                    continue
                results.append(
                    Message(role="tool", tool_call_id=tc.id, content=INTERRUPTED_RESULT)
                )
                self._sink.tool_finished(
                    tc, INTERRUPTED_RESULT, success=False, model=model
                )

            synthesized = sum(1 for item in raw if not isinstance(item, Message))
            log.info(f"exec_tool_calls: interrupted ({synthesized} synthesized)")
            raise InterruptedToolResults(results, original=cancel_err) from None

    async def _execute_one(self, tc: ToolCall, model: str) -> str:
        """执行单个工具调用：hook → 参数过滤 → 调用。"""
        self._sink.tool_started(tc)

        tool = self._tools.get(tc.name)
        if not tool:
            result = (
                f"Error: unknown tool: {tc.name}, "
                f"or you don't have permission to use it."
            )
            self._sink.tool_finished(tc, result, success=False, model=model)
            return result

        try:
            # Hook: before_tool_call
            modified_tc = await hooks.invoke_async("before_tool_call", tc)
            if modified_tc is not None:
                tc = modified_tc

            # 参数过滤：远程工具（VAR_KEYWORD）全量透传
            sig = inspect.signature(tool.function)
            has_var_keyword = any(
                p.kind is inspect.Parameter.VAR_KEYWORD for p in sig.parameters.values()
            )
            if has_var_keyword:
                call_args = dict(tc.arguments)
            else:
                actual_params = set(sig.parameters.keys())
                call_args = {
                    k: v for k, v in tc.arguments.items() if k in actual_params
                }

            result = (
                await tool.function(**call_args)
                if inspect.iscoroutinefunction(tool.function)
                else tool.function(**call_args)
            )
            result_str = str(result)

            # Hook: after_tool_call
            modified_result = await hooks.invoke_async(
                "after_tool_call",
                result_str,
                tool_name=tc.name,
                tool_args=tc.arguments,
                tool_call_id=tc.id,
            )
            if modified_result is not None:
                result_str = modified_result

            self._sink.tool_finished(tc, result_str, success=True, model=model)
            return result_str
        except ToolError as e:
            result = str(e)
            self._sink.tool_finished(tc, result, success=False, model=model)
            return result
        except Exception as e:
            result = f"Error executing tool '{tc.name}': {e}"
            self._sink.tool_finished(tc, result, success=False, model=model)
            return result


def _maybe_truncate(result: str) -> str:
    """Truncate tool result if it exceeds configured threshold.

    Full result is saved to a temp file so the agent can read it
    back via the Read tool if needed.
    """
    cfg = get_config().tool_result_truncate
    if cfg.max_length is None or cfg.max_length < 0:
        return result
    if len(result) <= cfg.max_length:
        return result

    keep = min(cfg.keep_chars, len(result) // 2)

    try:
        tmp_dir = get_wing_home() / "tmp"
        tmp_dir.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(
            mode="w",
            suffix=".txt",
            prefix="wing_truncated_",
            dir=str(tmp_dir),
            delete=False,
            encoding="utf-8",
        ) as f:
            f.write(result)
            full_path = Path(f.name)
        file_note = f"full result saved to: {full_path}. Use Read tool to view it."
    except OSError:
        log.warning("Failed to save truncated tool result to temp file")
        file_note = "full result could not be saved to disk."

    head = result[:keep]
    tail = result[-keep:] if keep > 0 else ""
    marker = f"... [truncated, original length: {len(result)} chars, {file_note}]"
    return f"{head}\n{marker}\n{tail}"
