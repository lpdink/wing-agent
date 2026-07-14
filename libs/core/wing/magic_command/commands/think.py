# wing/magic_command/commands/think.py

from typing import TYPE_CHECKING

from wing.event import SessionStateChangedEvent

from ..registry import magic_registry

if TYPE_CHECKING:
    from wing.agent import WingAgent

EFFORT_VALUES = frozenset({"low", "medium", "high", "xhigh", "max"})


@magic_registry.register(
    name="think",
    aliases=["t"],
    description="开启/关闭思考模式，或设置推理力度",
    params="on|off|low|medium|high|xhigh|max",
)
async def cmd_think(agent: "WingAgent", args: str) -> str:
    token = args.strip().lower()

    # off
    if token in ("off", "false", "0"):
        old = agent.model_provider.thinking
        agent.model_provider.set_thinking(False)
        agent.emit(
            SessionStateChangedEvent(session_id=agent.session_id, thinking=False)
        )
        return f"think: {old} → False"

    # on (不改变 reasoning_effort)
    if token in ("on", "true", "1"):
        old = agent.model_provider.thinking
        agent.model_provider.set_thinking(True)
        agent.emit(SessionStateChangedEvent(session_id=agent.session_id, thinking=True))
        effort = agent.model_provider.reasoning_effort or "default"
        return f"think: {old} → True (effort={effort})"

    # low/medium/high/xhigh/max
    if token in EFFORT_VALUES:
        agent.model_provider.set_thinking(True)
        agent.model_provider.set_reasoning_effort(token)
        agent.emit(SessionStateChangedEvent(session_id=agent.session_id, thinking=True))
        return f"think: True (effort={token})"

    # 无参数 → 显示当前状态
    if not token:
        thinking = agent.model_provider.thinking
        effort = agent.model_provider.reasoning_effort or "default"
        return f"think: {thinking} (effort={effort})"

    return "Usage: /think [on|off|low|medium|high|xhigh|max]"
