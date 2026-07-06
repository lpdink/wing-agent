import os

from wing.agent import WingAgent
from wing.schema import ToolError
from wing.tool_registry import tool_registry


def resolve_path(path: str, agent: WingAgent | None = None) -> str:
    """Resolve a path relative to the agent's cwd, falling back to process cwd.

    Shared by file/search tools so relative paths resolve against the session
    workspace directory.
    """
    if os.path.isabs(path):
        return path
    if agent is not None:
        cwd = agent.state.get("cwd")
        if isinstance(cwd, str):
            return os.path.join(cwd, path)
    return os.path.abspath(path)


@tool_registry.register()
async def timer(
    delay_seconds: float,
    reminder: str,
    agent: WingAgent,
) -> str:
    """Schedule a future wakeup for you.
    When the delay expires, you will receive the reminder as a new message.
    You must then respond appropriately based on its content.

    Note: After receiving confirmation, wait for the trigger — do not call this tool again for the same intent.

    Args:
        delay_seconds: Seconds until wakeup. Must be > 0.
        reminder: Context-rich note to your future self describing what
            has become due and how to proceed. Include relevant background
            so you can act without ambiguity.
    Returns:
        Timer confirmation message.
    Example:
        >>> await timer(300.0, "Follow up with user about deployment status")
        'Timer set: in 300.0s'
        # After 300s, you receive "[Timer] Follow up with user..."
        # You then ask the user about deployment status.
    """
    if delay_seconds <= 0:
        raise ToolError(
            f"Timer failed: delay_seconds must be positive, got {delay_seconds}"
        )
    agent.schedule_wakeup(delay_seconds, f"[Timer] {reminder}")
    return f"Timer set ok: in {delay_seconds}s"


# @tool_registry.register()
async def clear_context(agent: WingAgent) -> str:
    """Erase your entire conversation history and working memory.

    Use this when:
    - The current context has become irrelevant, corrupted, or contradictory
    - You need to abandon a failed approach and start fresh
    - User explicitly requests a reset / "forget everything"
    - Context window pressure requires emergency compaction

    WARNING: This is destructive and irreversible. You will lose:
    - All prior messages in this conversation
    - All accumulated state, plans, and intermediate results
    - Any pending timers or scheduled callbacks tied to this context

    After calling this, you will awaken with no memory of what came before.
    The user may need to re-explain their goal.

    Returns:
        Confirmation that context was cleared.

    Example:
        >>> await clear_context(agent)
        'clear_context:ok'
        # You now have zero context. Treat next input as a fresh conversation.
    """
    # agent.context.clear()
    return "clear_context:ok"
