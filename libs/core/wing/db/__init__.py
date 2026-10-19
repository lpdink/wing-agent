"""wing/db — 进程级 SQLite 门面（``$WING_HOME/core/wing.db``）。

与会话持久化正交：``wing/store/`` 管会话文件（metadata / history / aux），这里只管
进程级旁路数据（审计行、唤醒触发器）。**打开失败即降级**（``ready=False``），读写作
no-op——DB 坏了不能把网关带崩。

生命周期：进程单例、懒打开；组合根收尾时 ``close_database()``（幂等）。
"""

from __future__ import annotations

from wing.config import get_wing_home
from wing.db.database import Database

__all__ = ["Database", "close_database", "get_database", "reset_database"]

#: 库文件名（落在 ``get_wing_home()`` 里，即 ``$WING_HOME/core/wing.db``）。
DB_FILENAME = "wing.db"

_database: Database | None = None


def get_database() -> Database:
    """进程单例（首次调用时打开；打开失败也返回同一个降级对象）。"""
    global _database
    if _database is None:
        _database = Database(get_wing_home() / DB_FILENAME)
    return _database


def close_database() -> None:
    """关闭并丢弃单例（幂等；进程收尾调用）。"""
    global _database
    database, _database = _database, None
    if database is not None:
        database.close()


def reset_database() -> None:
    """测试用：关掉单例，让下次 ``get_database()`` 按当前环境重新打开。

    与 :func:`close_database` 同一实现——区别只在意图（测试隔离 vs 进程收尾）。
    """
    close_database()
