# wing/event_bus.py
"""
wing/event_bus.py — 全局事件总线

单例 EventBus，所有事件统一投递。
维护路由表 (client_id → set of session_ids)，emit 时根据事件 session_id
查路由表计算 EventTarget，注入到事件中供 Gateway 转发。

设计约束：
  - 单例：全局唯一，被所有模块引用
  - 同步 emit：不阻塞 agent，subscriber 自行调度异步
  - scope 由投递者指定：投递者知道事件应该发给谁
    - scope="session"：发给订阅了该 session_id 的所有 client
    - scope="global"：发给所有 client
  - request_id / session_id 由 RequestContext 维护，
    WingRuntime 在协程开头 set_request_context()
"""

from __future__ import annotations

from collections.abc import Callable
from typing import TYPE_CHECKING

from wing.common.logger import log
from wing.request_context import get_request_context

if TYPE_CHECKING:
    from wing.event import WingEvent


Subscriber = Callable[["WingEvent"], None]


class EventBus:
    """全局事件总线——单例。

    维护路由表 {client_id: set[session_id]}。
    emit 时根据 event.scope + event.session_id 查路由表计算 EventTarget，
    注入到 event.target 中供 subscriber（Gateway）转发。

    route_attach / route_detach 是路由表的维护操作，不是事件。

    subscriber callback 签名：Callable[WingEvent]
    event.target 已被注入，callback 从 event.target 读取路由信息。
    """

    def __init__(self) -> None:
        self._subscribers: list[Subscriber] = []
        self._routing: dict[str, set[str]] = {}  # client_id → set of session_ids

    def emit(self, event: "WingEvent") -> None:
        """投递事件：根据 scope 查路由表，计算 EventTarget，注入到 event.target。

        scope 由投递者指定：
          - "session"：从 RequestContext 取 session_id，查路由表得到 client_ids，
            构造 EventTarget(scope="client", client_ids=[...])
          - "global"：构造 EventTarget(scope="global)
          - "client"：投递者直接指定 client_ids

        Gateway 只看到 "global" 或 "client" + client_ids，不感知 session_id。
        """
        from wing.event import EventTarget

        # 从 RequestContext 注入 request_id / session_id
        ctx = get_request_context()
        if ctx.request_id is not None:
            event.request_id = ctx.request_id
        if ctx.session_id is not None and event.session_id is None:
            event.session_id = ctx.session_id

        # 计算 EventTarget
        if event.target is None:
            event.target = EventTarget(scope="global", client_ids=[])
            scope = "global"
        else:
            scope = event.target.scope

        if scope == "session":
            sid = event.session_id
            client_ids: list[str] = []
            if sid is not None:
                for cid, sids in self._routing.items():
                    if sid in sids:
                        client_ids.append(cid)
            event.target = EventTarget(scope="client", client_ids=client_ids)

        elif scope == "global":
            pass

        elif scope == "client":
            pass

        for callback in self._subscribers:
            try:
                callback(event)
            except Exception as e:
                log.error(f"EventBus subscriber error: {e}")

    def subscribe(self, callback: Subscriber) -> None:
        self._subscribers.append(callback)

    def unsubscribe(self, callback: Subscriber) -> None:
        try:
            self._subscribers.remove(callback)
        except ValueError:
            log.warning(f"Attempted to unsubscribe unknown callback: {callback}")

    def route_attach(self, client_id: str, session_id: str) -> None:
        self._routing.setdefault(client_id, set()).add(session_id)

    def route_detach(self, client_id, session_id: str) -> None:
        if client_id in self._routing:
            self._routing[client_id].discard(session_id)
            if not self._routing[client_id]:
                del self._routing[client_id]

    def route_detach_client(self, client_id: str) -> None:
        self._routing.pop(client_id, None)

    @property
    def routing_table(self) -> dict[str, set[str]]:
        return dict(self._routing)

    @property
    def subscriber_count(self) -> int:
        return len(self._subscribers)


# 全局单例
event_bus = EventBus()
