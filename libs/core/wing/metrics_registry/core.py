"""wing.metrics_registry._core — MetricsRegistry 类、单例、原子写入工具。

单独文件避免 __init__.py 与 handler 模块的循环导入。
handler 模块从这里导入 metrics_registry 单例和工具函数，
__init__.py 从 handler 模块导入 handler（触发装饰器注册），无循环。
"""

from __future__ import annotations

import json
import os
import time as time_mod
from collections import defaultdict
from collections.abc import Callable
from pathlib import Path
from typing import Any, TypeVar

from wing.common.logger import log
from wing.event import WingEvent

E = TypeVar("E", bound=WingEvent)


# ============================================================
# 原子写入 & 读取工具
# ============================================================


def _atomic_write_json(path: Path, data: dict[str, Any]) -> None:
    """原子写入 JSON 文件。先写 tmp 再 rename，确保写入不丢失。"""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(f".tmp.{os.getpid()}")
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(data, f, ensure_ascii=False, indent=2)
        f.flush()
        os.fsync(f.fileno())
    os.rename(tmp, path)


def _read_metrics_json(path: Path) -> dict[str, Any]:
    """读取 metrics.json，文件不存在时返回空 dict。

    JSON 损坏时备份损坏文件（加 .corrupted.{timestamp} 后缀），
    防止后续写入覆盖导致历史数据永久丢失。
    """
    if path.exists():
        try:
            with open(path, encoding="utf-8") as f:
                return json.load(f)
        except json.JSONDecodeError as e:
            log.error(f"Corrupted metrics.json at {path}: {e}")
            backup_path = path.with_suffix(f".corrupted.{int(time_mod.time())}")
            path.rename(backup_path)
            log.warning(f"Backed up corrupted file to {backup_path}")
        except OSError as e:
            log.warning(f"Failed to read metrics.json: {e}")
    return {}


# ============================================================
# MetricsRegistry
# ============================================================


class MetricsRegistry:
    """审计注册中心。

    支持装饰器模式注册 handler，按事件类型（isinstance）分发。
    """

    def __init__(self) -> None:
        self._handlers: dict[type[WingEvent], list[Callable[..., None]]] = defaultdict(
            list
        )

    def on(
        self, event_cls: type[E]
    ) -> Callable[[Callable[[E], None]], Callable[[E], None]]:
        """装饰器：注册 handler 到指定事件类型。"""

        def decorator(handler: Callable[[E], None]) -> Callable[[E], None]:
            self._handlers[event_cls].append(handler)
            return handler

        return decorator

    def register(self, event_cls: type[E], handler: Callable[[E], None]) -> None:
        """注册 handler 到指定事件类型（编程式）。"""
        self._handlers[event_cls].append(handler)

    def handle(self, event: WingEvent) -> None:
        """事件处理入口。isinstance 路由，handler 异常不中断分发链。"""
        for event_cls, handlers in self._handlers.items():
            if isinstance(event, event_cls):
                for handler in handlers:
                    try:
                        handler(event)
                    except Exception as e:
                        log.error(
                            f"Metrics handler error for {event_cls.__name__}: {e}"
                        )


metrics_registry = MetricsRegistry()
