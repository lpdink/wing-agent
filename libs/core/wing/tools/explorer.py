# wing/tools/explorer.py
"""Explorer — read-only sub-agent tool for code exploration.

Creates a lightweight sub-agent with Read/Glob/Grep tools only.
By default runs in foreground (blocking) — the tool waits for the sub-agent
to finish and returns results directly. Set run_in_background=true to launch
asynchronously and get notified via inbox when complete.
"""

import asyncio
import re
from pathlib import Path

from wing.agent import WingAgent
from wing.common.tracked_list import TrackedList
from wing.common.utils import generate_session_id
from wing.config import get_config
from wing.context_manager import ContextManager
from wing.event import DoneEvent, WingEvent, TextEvent
from wing.event_bus import event_bus
from wing.schema import Message
from wing.store import FileMessageLog
from wing.tool_registry import tool_registry

_EXPLORER_SYSTEM_PROMPT = (
    "You are an Explorer agent. Your job is to read, search, and analyze "
    "code or files as instructed, then report findings concisely and "
    "accurately.\n\n"
    "Guidelines:\n"
    "- Focus on the specific task described in the user message\n"
    "- Use Read, Glob, and Grep tools to gather information\n"
    "- Report findings in a clear, structured format\n"
    "- Include relevant file paths and line numbers\n"
    "- Do not speculate — only report what you actually find"
)

_TIMEOUT_SECONDS = 1200  # 20 minutes

# Track active background tasks for potential cleanup.
# TODO: add shutdown hook to cancel pending tasks on process exit; currently
#       sub-agent shutdown() is skipped if the process dies mid-flight.
_active_explorers: set[asyncio.Task] = set()


@tool_registry.register(name="Explorer", add_purpose=True)
async def explorer_agent(
    name: str,
    task_detail: str,
    run_in_background: bool = False,
    agent: WingAgent | None = None,
) -> str:
    """Launch a read-only Explorer sub-agent to investigate code or files.

    The Explorer has access to Read, Glob, and Grep tools only.

    By default this tool blocks until the Explorer finishes and returns
    results directly. Set run_in_background=true to launch asynchronously
    and continue with other work — you will be notified when it completes.

    IMPORTANT: The Explorer has NO context about your conversation.
    Provide a self-contained task description with absolute paths and goals.

    Args:
        name: Short task name (3-5 words).
        task_detail: Detailed, self-contained exploration task description.
        run_in_background: Set to true to run in background. Default false (blocking).
    """
    assert agent is not None  # always injected by tool_registry

    safe_name = _sanitize_name(name)
    sub_sid = generate_session_id()

    sessions_path = get_config().sessions.resolved_path()
    parent_dir = sessions_path / agent.session_id
    sub_dir = parent_dir / "subagents" / sub_sid
    result_path = sub_dir / f"{safe_name}_result.md"

    # TrackedList — persists sub-agent message history
    # TODO(remote-workspace): 子 agent 历史目前显式使用文件后端，
    # 随远程工具注册改经 session 所属 store。
    messages: TrackedList[Message] = TrackedList(FileMessageLog(sub_dir))

    # ContextManager — no skills, no rules, inherits parent's compactor
    cm = ContextManager(
        session_id=sub_sid,
        messages=messages,
        system_prompt=_EXPLORER_SYSTEM_PROMPT,
        compactor=agent.context_manager.compactor,
    )

    # Sub-agent — same model, stream=False, read-only tools
    ro_tools = [tool_registry.resolve(n) for n in ("Read", "Glob", "Grep")]
    sub_agent = WingAgent(
        model=agent.model,
        model_provider=agent.model_provider,
        stream=False,
        context_manager=cm,
        tools=[t for t in ro_tools if t is not None],
    )

    if run_in_background:
        # Fire-and-forget: run in background, return immediately.
        task = asyncio.create_task(
            _run_explorer_background(
                sub_agent,
                sub_sid,
                sub_dir,
                name,
                safe_name,
                task_detail,
                result_path,
                agent,
            )
        )
        _active_explorers.add(task)
        task.add_done_callback(_active_explorers.discard)

        return (
            f"Explorer task '{name}' launched in background (session: {sub_sid}). "
            "You will be notified when results are ready. "
            "Continue with other work in the meantime."
        )

    # ── Foreground (blocking) path ──
    try:
        content_parts = await _collect_explorer_output(sub_agent, sub_sid, task_detail)
        _write_result(sub_dir, safe_name, content_parts)
        body = (
            "\n".join(content_parts)
            if content_parts
            else "Explorer returned no results."
        )
        return body
    except asyncio.TimeoutError:
        _write_result(sub_dir, safe_name, [])
        return (
            f"Explorer task '{name}' timed out after {_TIMEOUT_SECONDS}s. "
            "No results were produced."
        )
    except Exception as e:
        return f"Explorer task '{name}' failed with error: {e}"
    finally:
        await sub_agent.shutdown()


async def _collect_explorer_output(
    sub_agent: WingAgent,
    sub_sid: str,
    task_detail: str,
) -> list[str]:
    """Run sub-agent and collect its text output until DoneEvent.

    Raises asyncio.TimeoutError if the sub-agent exceeds _TIMEOUT_SECONDS.
    """
    done_event = asyncio.Event()
    content_parts: list[str] = []

    def on_event(event: WingEvent) -> None:
        if event.session_id != sub_sid:
            return
        if isinstance(event, TextEvent):
            content_parts.append(event.content)
        elif isinstance(event, DoneEvent):
            done_event.set()

    event_bus.subscribe(on_event)
    try:
        await sub_agent.post(task_detail)
        await asyncio.wait_for(done_event.wait(), timeout=_TIMEOUT_SECONDS)
    finally:
        event_bus.unsubscribe(on_event)

    return content_parts


async def _run_explorer_background(
    sub_agent: WingAgent,
    sub_sid: str,
    sub_dir: Path,
    name: str,
    safe_name: str,
    task_detail: str,
    result_path: Path,
    parent_agent: WingAgent,
) -> None:
    """Background task: run explorer, write results, notify parent via post()."""
    try:
        content_parts = await _collect_explorer_output(sub_agent, sub_sid, task_detail)
        _write_result(sub_dir, safe_name, content_parts)
        await parent_agent.post(
            f"[Explorer] Task '{name}' completed. "
            f"Results saved to: {result_path}. "
            f"Read the file for details."
        )
    except asyncio.TimeoutError:
        _write_result(sub_dir, safe_name, [])
        await parent_agent.post(
            f"[Explorer] Task '{name}' timed out after {_TIMEOUT_SECONDS}s."
        )
    except Exception as e:
        await parent_agent.post(f"[Explorer] Task '{name}' failed with error: {e}")
    finally:
        await sub_agent.shutdown()


def _sanitize_name(name: str) -> str:
    """Sanitize task name for safe use as a filename component.

    Replaces whitespace with hyphens, strips path separators and traversal
    sequences, and removes characters outside [a-zA-Z0-9._-].
    """
    # Collapse whitespace (spaces, tabs, etc.) into single hyphen
    s = re.sub(r"\s+", "-", name.strip())
    # Remove path separators and traversal
    s = s.replace("/", "").replace("\\", "").replace("..", "")
    # Keep only safe filename characters
    s = re.sub(r"[^a-zA-Z0-9._-]", "", s)
    # Collapse multiple hyphens
    s = re.sub(r"-{2,}", "-", s).strip("-")
    return s or "explorer"


def _write_result(dir_path: Path, name: str, parts: list[str]) -> None:
    """Write collected content to a result markdown file."""
    dir_path.mkdir(parents=True, exist_ok=True)
    body = "\n".join(parts) if parts else "No results collected."
    (dir_path / f"{name}_result.md").write_text(body, encoding="utf-8")
