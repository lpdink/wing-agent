from .tools import execute_shell as execute_shell
from . import metrics_registry as metrics_registry  # 触发模块级 event_bus.subscribe

__all__ = ["execute_shell"]
