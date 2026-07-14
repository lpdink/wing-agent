# wing/magic_command/commands/yolo.py

from typing import TYPE_CHECKING

from wing.event import SessionStateChangedEvent

from ..registry import magic_registry
from .utils import parse_bool_arg

if TYPE_CHECKING:
    from wing.agent import WingAgent


@magic_registry.register(
    name="yolo", description="切换 YOLO 模式（跳过危险命令审查）", params="on|off"
)
async def cmd_yolo(agent: "WingAgent", args: str) -> str:
    if not args.strip():
        return f"yolo: {agent.yolo}"

    new_yolo = parse_bool_arg(args)
    if new_yolo is None:
        return "Usage: /yolo on|off"

    old_val = agent.yolo
    agent.set_yolo(new_yolo)
    agent.emit(SessionStateChangedEvent(session_id=agent.session_id, yolo=new_yolo))
    return f"yolo: {old_val} → {new_yolo}"
