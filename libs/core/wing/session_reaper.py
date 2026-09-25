# wing/session_reaper.py
"""SessionReaper — 空闲会话逐出。

语义：session 的内存态是**缓存**。逐出只回收运行期资源（worker task +
provider client），磁盘状态一概不动——被逐出的会话在下一次被需要时按需
水合（``SessionManager.ensure_loaded`` / resume）。

逐出判定（三条全过才逐出，见 ``SessionManager.evict_idle_sessions``）：

  1. **空闲**：``status == idle``（working / waiting 一律钉住）；
  2. **无订阅**：EventBus 路由表里没有 client 订阅该会话；
  3. **空闲时长 > idle_ttl_seconds**：计时器由"会话状态变化"重置——
     任何携带该 session_id 的事件都会 ``touch`` 一次（见 ``_on_event``）。

另有两条硬性不逐出条件：有后台任务在跑（拆解会关掉它正在用的 provider）、
非持久后端（memory 后端逐出 = 数据销毁）。

节奏由 ``BackgroundScheduler`` 驱动（gateway lifespan 注册 job）；本类只做
"触摸订阅 + 一次扫描"，不依赖调度器即可单测。
"""

from __future__ import annotations

from wing.common.logger import log
from wing.config import get_config
from wing.event import WingEvent
from wing.event_bus import event_bus
from wing.session_manager import SessionManager


class SessionReaper:
    """空闲会话逐出：EventBus 触摸 + 一次扫描。"""

    def __init__(self, session_manager: SessionManager) -> None:
        self._sm = session_manager
        self._attached = False

    @property
    def attached(self) -> bool:
        """是否已订阅 EventBus（触摸通道生效中）。"""
        return self._attached

    def attach(self) -> None:
        """订阅 EventBus：任何会话事件都视为状态变化（重置空闲计时器）。"""
        if self._attached:
            return
        event_bus.subscribe(self._on_event)
        self._attached = True
        log.info("SessionReaper attached to EventBus")

    def detach(self) -> None:
        """退订 EventBus（幂等）。"""
        if not self._attached:
            return
        event_bus.unsubscribe(self._on_event)
        self._attached = False

    def _on_event(self, event: WingEvent) -> None:
        """EventBus 回调：刷新事件所属会话的空闲计时器。"""
        if event.session_id:
            self._sm.touch(event.session_id)

    async def sweep(self) -> list[str]:
        """扫描一轮并按 TTL 逐出空闲会话，返回被逐出的 session id 列表。"""
        config = get_config().sessions.eviction
        if not config.enabled:
            return []
        evicted = self._sm.evict_idle_sessions(ttl_seconds=config.idle_ttl_seconds)
        if evicted:
            log.info(f"Session eviction sweep: {len(evicted)} session(s) released")
        return evicted
