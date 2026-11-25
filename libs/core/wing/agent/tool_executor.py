# wing/agent/tool_executor.py
"""ToolExecutor — 工具并发执行 + 中断收尸。

职责：
- 并发调度多个 tool call（asyncio.gather）
- 中断时收拢每个 call 的最终结果（真实或合成），保证上下文不悬空
- 单工具执行：非法参数短路 → hook → 参数过滤 → 调用 → 截断
- 事件通过 AgentEventSink 发射
"""

from __future__ import annotations

import asyncio
import inspect
import tempfile
import time
from collections.abc import Iterator
from contextlib import contextmanager
from contextvars import ContextVar
from dataclasses import dataclass
from datetime import UTC, datetime
from functools import partial
from pathlib import Path

from wing.common.logger import log
from wing.common.truncate import head_tail_marker
from wing.config import get_config, get_wing_home
from wing.event import ToolExecutionTiming
from wing.hooks import hooks
from wing.schema import MediaRef, Message, Tool, ToolCall, ToolError, ToolOutput
from wing.schema.message import ToolResultStatus

from .event_sink import AgentEventSink
from .tool_context import _tool_execution_measurement

# 当前正在执行的工具调用 id。execute() 为每个工具任务设置（gather 各 task
# 的 context 相互隔离），ask_feedback() 据此把 AskEvent 与 feedback waiter 关联。
_current_tool_call_id: ContextVar[str | None] = ContextVar(
    "current_tool_call_id", default=None
)


@dataclass
class _ToolExecutionClock:
    started_at: float
    timeout_secs: float | None
    duration_ms: int | None = None

    def elapsed_ms(self) -> int:
        return int((time.monotonic() - self.started_at) * 1000)

    def snapshot(self) -> ToolExecutionTiming:
        duration = self.duration_ms
        return ToolExecutionTiming(
            elapsed_ms=self.elapsed_ms() if duration is None else duration,
            running=duration is None,
            timeout_secs=self.timeout_secs,
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


def interrupted_tool_result(call: ToolCall) -> Message:
    message = Message(
        role="tool",
        tool_call_id=call.id,
        content=INTERRUPTED_RESULT,
        tool_status="interrupted",
        message_at=datetime.now(UTC),
    )
    message.assign_identity()
    return message


class ToolExecutor:
    """工具并发执行器。"""

    def __init__(self, sink: AgentEventSink) -> None:
        self._sink = sink
        self._tools: dict[str, Tool] = {}
        self._tool_clocks: dict[str, _ToolExecutionClock] = {}
        self._tool_results: dict[str, Message] = {}

    def tool_execution_timings(self) -> dict[str, ToolExecutionTiming]:
        return {
            call_id: clock.snapshot() for call_id, clock in self._tool_clocks.items()
        }

    def uncommitted_tool_results(self) -> list[Message]:
        return list(self._tool_results.values())

    @contextmanager
    def _measure_tool_execution(
        self, clocks: dict[str, _ToolExecutionClock], call_id: str, timeout: float
    ) -> Iterator[None]:
        if clocks is not self._tool_clocks:
            yield
            return

        # Tool arguments arrive from JSON without coercion against annotations.
        budget = timeout if isinstance(timeout, (int, float)) else None
        clock = clocks[call_id] = _ToolExecutionClock(time.monotonic(), budget)
        self._sink.tool_execution_timing(call_id, clock.snapshot())
        # A normal return or a wait timeout reaches a known measurement boundary.
        completed = True
        try:
            yield
        except BaseException as error:
            completed = isinstance(error, TimeoutError)
            raise
        finally:
            if (
                completed
                and clocks is self._tool_clocks
                and clocks.get(call_id) is clock
            ):
                clock.duration_ms = clock.elapsed_ms()
                self._sink.tool_execution_timing(call_id, clock.snapshot())

    def set_tools(self, tools: dict[str, Tool]) -> None:
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

        # Batch-local ownership keeps late cancellation cleanup off the next batch's map.
        clocks: dict[str, _ToolExecutionClock] = {}
        self._tool_clocks = clocks
        completed: dict[str, Message] = {}
        self._tool_results = completed

        async def _exec_one(tc: ToolCall) -> Message:
            call_id = tc.id
            log.info(f"exec_tool_calls: executing tool '{tc.name}' with id={call_id}")
            token = _current_tool_call_id.set(call_id)
            try:
                result = await self._execute_one(tc, model, clocks)
            except Exception as e:
                log.error(f"exec_tool_calls: unexpected error in tool '{tc.name}': {e}")
                content = f"Error executing tool '{tc.name}': {e}"
                result = Message(
                    role="tool",
                    tool_call_id=call_id,
                    content=content,
                    tool_status="error",
                    message_at=datetime.now(UTC),
                )
                result.assign_identity()
                self._sink.tool_finished(
                    tc, content, success=False, model=model, message=result
                )
            finally:
                clocks.pop(call_id, None)
                _current_tool_call_id.reset(token)
            completed[call_id] = result
            return result

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
                result = interrupted_tool_result(tc)
                results.append(result)
                self._sink.tool_finished(
                    tc, INTERRUPTED_RESULT, success=False, model=model, message=result
                )

            synthesized = sum(1 for item in raw if not isinstance(item, Message))
            log.info(f"exec_tool_calls: interrupted ({synthesized} synthesized)")
            raise InterruptedToolResults(results, original=cancel_err) from None
        finally:
            clocks.clear()
            if self._tool_clocks is clocks:
                self._tool_clocks = {}
                self._tool_results = {}

    async def _execute_one(
        self, tc: ToolCall, model: str, clocks: dict[str, _ToolExecutionClock]
    ) -> Message:
        """执行单个工具调用：非法参数短路 → hook → 参数过滤 → 调用。

        工具可返回 ``str``（存量，零改动）或 ``ToolOutput``（文本 + 媒体
        引用）。``after_tool_call`` hook 只接收/改写文本，media 原样透传。
        每个终态都把实测时长（毫秒）交给 tool_finished——会话用量账本据此
        累计工具总时长；**没有真正执行**的两条短路（参数非法 / 未知工具）
        交 ``duration_ms=None``（账本按「不是一次执行」处理），中断合成的
        结果由 execute() 直接发射、不经这里，同样不会被记成一次执行。
        """
        call_id = tc.id
        self._sink.tool_started(tc)
        started = time.monotonic()
        arguments = tc.arguments

        def finish(
            content: str,
            *,
            status: ToolResultStatus,
            media: list[MediaRef] | None = None,
            executed: bool = True,
        ) -> Message:
            measured_at = datetime.now(UTC)
            duration_ms = int((time.monotonic() - started) * 1000) if executed else None
            message = Message(
                role="tool",
                tool_call_id=call_id,
                content=_maybe_truncate(content),
                media=media or None,
                tool_status=status,
                duration_ms=duration_ms,
                message_at=measured_at,
                tool_args=arguments if arguments != tc.arguments else None,
            )
            message.assign_identity()
            self._sink.tool_finished(
                tc.model_copy(update={"arguments": arguments}),
                content,
                success=status == "success",
                model=model,
                media=media,
                duration_ms=duration_ms,
                message=message,
            )
            return message

        # 参数 JSON 解析失败（provider 已容错并置 arguments_error）：不执行
        # 工具，把错误现场作为结果回灌给模型自纠——与 unknown-tool 同级的
        # 正常失败路径，MUST NOT 抛异常（抛了会触发整轮重试，丢弃本轮
        # thinking/content/tool call）。
        if tc.arguments_error:
            result = (
                f"Error: tool call '{tc.name}' was NOT executed — the arguments "
                f"you generated are invalid.\n{tc.arguments_error}\n"
                f"Please re-issue the tool call with corrected arguments."
            )
            return finish(result, status="error", executed=False)

        tool = self._tools.get(tc.name)
        if not tool:
            result = (
                f"Error: unknown tool: {tc.name}, "
                f"or you don't have permission to use it."
            )
            return finish(result, status="error", executed=False)

        try:
            modified_tc = await hooks.invoke_async(
                "before_tool_call", tc.model_copy(deep=True)
            )
            if modified_tc is not None:
                arguments = modified_tc.arguments

            # 参数过滤：远程工具（VAR_KEYWORD）全量透传
            sig = inspect.signature(tool.function)
            has_var_keyword = any(
                p.kind is inspect.Parameter.VAR_KEYWORD for p in sig.parameters.values()
            )
            if has_var_keyword:
                call_args = dict(arguments)
            else:
                actual_params = set(sig.parameters.keys())
                call_args = {k: v for k, v in arguments.items() if k in actual_params}

            measurement_token = _tool_execution_measurement.set(
                partial(self._measure_tool_execution, clocks, call_id)
            )
            try:
                result = (
                    await tool.function(**call_args)
                    if inspect.iscoroutinefunction(tool.function)
                    else tool.function(**call_args)
                )
            finally:
                _tool_execution_measurement.reset(measurement_token)
            if isinstance(result, ToolOutput):
                content = result.content
                media = list(result.media)
            else:
                content = str(result)
                media = []

            # hook 只见文本——media 是引用元数据，hook 无权改写
            modified_result = await hooks.invoke_async(
                "after_tool_call",
                content,
                tool_name=tc.name,
                tool_args=arguments,
                tool_call_id=tc.id,
            )
            if modified_result is not None:
                content = modified_result

        except ToolError as e:
            return finish(str(e), status="error")
        except Exception as e:
            return finish(f"Error executing tool '{tc.name}': {e}", status="error")
        return finish(content, status="success", media=media)


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

    return head_tail_marker(result, keep_head=keep, keep_tail=keep, note=file_note)
