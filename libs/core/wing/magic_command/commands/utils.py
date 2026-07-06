# wing/magic_command/commands/utils.py — 魔术命令共用工具函数

"""魔术命令间共享的辅助函数和常量."""

from typing import TYPE_CHECKING

from wing.event import ContextStatsEvent

if TYPE_CHECKING:
    from wing.agent import WingAgent

TRUE_LIST = frozenset({"on", "true", "1"})
FALSE_LIST = frozenset({"off", "false", "0"})


def parse_bool_arg(args: str) -> bool | None:
    """Parse a boolean argument. Returns True/False or None if invalid."""
    token = args.strip().lower()
    if token in TRUE_LIST:
        return True
    if token in FALSE_LIST:
        return False
    return None


def emit_context_stats(agent: "WingAgent") -> None:
    """emit ContextStatsEvent — 多个命令在改变上下文后共用此逻辑."""
    cm = agent.context_manager
    count, tokens = cm.get_context_stats()
    ctx_window = 0
    if cm.compactor:
        ctx_window = cm.compactor.context_window_tokens
    agent.emit(
        ContextStatsEvent(
            session_id=agent.session_id,
            message_count=count,
            total_tokens=tokens,
            context_window_tokens=ctx_window,
        )
    )
