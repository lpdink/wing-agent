# wing/agent/tool_context.py
"""ToolContext — 工具侧窄接口 Protocol。

工具函数通过此 Protocol 访问 agent 能力，而非持有 WingAgent 全量引用。
WingAgent 实现此 Protocol（结构化子类型，无需显式继承）。
"""

from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
from typing import TYPE_CHECKING, Protocol, runtime_checkable

if TYPE_CHECKING:
    from wing.event import AskEvent, WingEvent


@runtime_checkable
class ToolContext(Protocol):
    """工具执行时可用的 agent 能力窄接口。"""

    @property
    def session_id(self) -> str: ...

    @property
    def yolo(self) -> bool: ...

    @property
    def cwd(self) -> Path | None: ...

    def set_yolo(self, value: bool) -> None: ...

    def set_cwd(self, path: Path | None) -> None: ...

    async def ask_feedback(self, event: AskEvent, timeout: float) -> str: ...

    def emit(self, event: WingEvent) -> None: ...

    def register_interrupt_hook(self, hook: Callable[[], None]) -> str: ...

    def unregister_interrupt_hook(self, hook_id: str) -> None: ...
