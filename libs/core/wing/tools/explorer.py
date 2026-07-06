# wing/tools/explorer.py
"""Explorer — read-only sub-agent tool for code exploration.

Creates a lightweight sub-agent with Read/Glob/Grep tools only.
Runs asynchronously — the tool returns immediately with a launch confirmation,
and the parent agent is notified via inbox when the sub-agent completes.
"""

import asyncio
from pathlib import Path

from wing.agent import WingAgent
from wing.common.tracked_list import TrackedList
from wing.common.utils import generate_session_id
from wing.config import get_config
from wing.context_manager import ContextManager
from wing.event import DoneEvent, WingEvent, TextEvent
from wing.event_bus import event_bus
from wing.schema import Message
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
    agent: WingAgent,
) -> str:
    """Launch a read-only Explorer sub-agent to investigate code or files.

    The Explorer has access to Read, Glob, and Grep tools only.
    Runs asynchronously — you will be notified when results are ready.

    IMPORTANT: The Explorer has NO context about your conversation.
    You MUST provide a self-contained, detailed task description including:
    - Absolute paths of directories/files to explore
    - Specific goals and what information to gather
    - Expected output format

    Args:
        name: Short task name (used as result filename).
        task_detail: Detailed, self-contained exploration task description.
    """
    sub_sid = generate_session_id()

    sessions_path = get_config().sessions.resolved_path()
    parent_dir = sessions_path / agent.session_id
    sub_dir = parent_dir / "subagents" / sub_sid
    result_path = sub_dir / f"{name}_result.md"

    # TrackedList — persists sub-agent message history
    messages: TrackedList[Message] = TrackedList(sub_dir)

    # ContextManager — no skills, no rules, inherits parent's compactor
    cm = ContextManager(
        session_id=sub_sid,
        messages=messages,
        system_prompt=_EXPLORER_SYSTEM_PROMPT,
        compactor=agent.context_manager.compactor,
    )

    # Sub-agent — same model, stream=False, read-only tools
    # NOTE: sub-agent events broadcast through the global EventBus just like
    #       the parent agent. TUI filters by session_id; other subscribers
    #       (e.g. Gateway) must do the same if they don't want sub-agent events.
    ro_tools = [tool_registry.get_tool(n) for n in ("Read", "Glob", "Grep")]
    sub_agent = WingAgent(
        model=agent.model,
        model_provider=agent.model_provider,
        stream=False,
        context_manager=cm,
        tools=[t for t in ro_tools if t is not None],
    )

    # Fire-and-forget: run in background, return immediately.
    task = asyncio.create_task(
        _run_explorer_background(
            sub_agent, sub_sid, sub_dir, name, task_detail, result_path, agent
        )
    )
    _active_explorers.add(task)
    task.add_done_callback(_active_explorers.discard)

    return (
        f"Explorer task '{name}' launched (session: {sub_sid}). "
        "Sub-agent results typically take several minutes. "
        "If you have other tasks, proceed with them or consider dispatching more sub-agents. "
        "If not, simply wait — you will be notified when results are ready."
    )


async def _run_explorer_background(
    sub_agent: WingAgent,
    sub_sid: str,
    sub_dir: Path,
    name: str,
    task_detail: str,
    result_path: Path,
    parent_agent: WingAgent,
) -> None:
    """Background task: run explorer, write results, notify parent via post()."""
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

        try:
            await asyncio.wait_for(done_event.wait(), timeout=_TIMEOUT_SECONDS)
        except asyncio.TimeoutError:
            _write_result(sub_dir, name, content_parts)
            await parent_agent.post(
                f"[Explorer] Task '{name}' timed out after {_TIMEOUT_SECONDS}s. "
                f"Partial results saved to: {result_path}"
            )
            return

        _write_result(sub_dir, name, content_parts)
        await parent_agent.post(
            f"[Explorer] Task '{name}' completed. "
            f"Results saved to: {result_path}. "
            f"Read the file for details."
        )
    except Exception as e:
        await parent_agent.post(f"[Explorer] Task '{name}' failed with error: {e}")
    finally:
        event_bus.unsubscribe(on_event)
        await sub_agent.shutdown()


def _write_result(dir_path: Path, name: str, parts: list[str]) -> None:
    """Write collected content to a result markdown file."""
    dir_path.mkdir(parents=True, exist_ok=True)
    body = "\n".join(parts) if parts else "No results collected."
    (dir_path / f"{name}_result.md").write_text(body, encoding="utf-8")
