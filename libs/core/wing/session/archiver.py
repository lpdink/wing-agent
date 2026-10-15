# wing/session/archiver.py
"""SessionArchiver — 会话自动归档。

语义：超过 ``sessions.archive.idle_days`` 未交互的会话移入归档区
（``<sessions root>/.archive/``）——不再参与 ``/api/session/list``，恢复必须
显式 unarchive。磁盘状态只是换了位置，字节不变。

判定（见 ``SessionManager.archive_stale_sessions``）：

  1. **年龄**：``metadata.last_interaction``（回退 session id 时间前缀）超过
     阈值；时间取不到 → 跳过（不发明时间）；
  2. **豁免**：带 ``pin`` 标签的会话不自动归档（``session.tags.PIN_TAG``——
     后端感知的唯一保留标签；显式归档不受限）；
  3. **可收回**：忙碌 / 有待处理输入 / 被订阅的会话跳过本轮（与逐出同一套
     钉住条件）；空闲可收回的先逐出再移动。

``idle_days <= 0`` = 关闭（不扫描）。节奏由 ``BackgroundScheduler`` 驱动
（启动延迟一轮 + 周期扫描，gateway lifespan 注册 job）；本类只做"读配置 +
一次扫描"，不依赖调度器即可单测。
"""

from __future__ import annotations

from wing.common.logger import log
from wing.config import get_config

from .manager import SessionManager


class SessionArchiver:
    """自动归档：读配置 + 一次扫描（无 EventBus 订阅，无状态）。"""

    def __init__(self, session_manager: SessionManager) -> None:
        self._sm = session_manager

    async def sweep(self) -> list[str]:
        """扫描一轮并归档过期会话，返回被归档的 session id 列表。

        每个候选的年龄与豁免都在 `archive_stale_sessions` 里终检（重读磁盘）；
        这里只负责读一次当前配置（热重载即时生效：阈值每轮现读）。
        """
        config = get_config().sessions.archive
        archived = await self._sm.archive_stale_sessions(config.idle_days)
        if archived:
            log.info(f"Session archive sweep: {len(archived)} session(s) archived")
        return archived
