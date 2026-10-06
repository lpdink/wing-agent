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
    from wing.config import ModelCapabilities
    from wing.event import AskEvent, WingEvent
    from wing.media import MediaAccess


@runtime_checkable
class ToolContext(Protocol):
    """工具执行时可用的 agent 能力窄接口。"""

    @property
    def session_id(self) -> str: ...

    @property
    def media(self) -> MediaAccess | None:
        """会话媒体读写窄接口（工具写图 / 读图）。

        None = 该 agent 无媒体存储（裸测试构造）——工具必须安全拒绝，
        不得假定可用。
        """
        ...

    @property
    def model(self) -> str:
        """当前模型的实际调用名（门禁文案用裸名，不带 provider 前缀）。"""
        ...

    @property
    def capabilities(self) -> ModelCapabilities:
        """当前模型的能力声明（读图门禁的唯一依据）。

        实时解析、无缓存——会话中 `/model` 切换后返回值随之变化。
        未声明 = text-only（`vision=False`），见 ``resolve_model_capabilities``。
        """
        ...

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
