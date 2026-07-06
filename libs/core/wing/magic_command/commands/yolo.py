# wing/magic_command/commands/yolo.py

from typing import TYPE_CHECKING

from wing.config import get_config

from ..registry import magic_registry
from .utils import parse_bool_arg

if TYPE_CHECKING:
    from wing.agent import WingAgent


def _effective_yolo(agent: "WingAgent") -> tuple[bool, str]:
    """返回 (有效值, 来源标签)。"""
    session_val = agent.state.get("yolo")
    if session_val is not None:
        return bool(session_val), "session"
    return get_config().yolo, "global"


@magic_registry.register(
    name="yolo", description="切换 YOLO 模式（跳过危险命令审查）", params="on|off"
)
async def cmd_yolo(agent: "WingAgent", args: str) -> str:
    if not args.strip():
        val, src = _effective_yolo(agent)
        return f"yolo: {val} ({src})"

    new_yolo = parse_bool_arg(args)
    if new_yolo is None:
        return "Usage: /yolo on|off"

    old_val, _ = _effective_yolo(agent)
    agent.state.set("yolo", new_yolo)
    return f"yolo: {old_val} → {new_yolo} (session)"
