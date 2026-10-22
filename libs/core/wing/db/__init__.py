"""wing/db — 进程级 SQLite 门面（``$WING_HOME/core/wing.db``）。

与会话持久化正交：``wing/store/`` 管会话文件（metadata / history / aux），这里只管
进程级旁路数据（审计行、唤醒触发器、缓存账本）。**打开失败即降级**（``ready=False``），
读写作 no-op——DB 坏了不能把网关带崩。

生命周期：进程单例、懒打开；组合根收尾时 ``close_database()``（幂等）。
"""

from __future__ import annotations

import threading

from wing.config import get_wing_home
from wing.db.database import Database

__all__ = ["Database", "close_database", "get_database", "reset_database"]

#: 库文件名（落在 ``get_wing_home()`` 里，即 ``$WING_HOME/core/wing.db``）。
DB_FILENAME = "wing.db"

_database: Database | None = None

#: 单例的构造锁。首次访问可能来自任一工作线程（缓存账本在 ``asyncio.to_thread``
#: 里记账）——两个线程同时发现「还没有库」就会各自打开一份连接，输家撞
#: ``database is locked`` 并把这次记账丢掉。
_lifecycle_lock = threading.Lock()


def get_database() -> Database:
    """进程单例（首次调用时打开；打开失败也返回同一个降级对象）。"""
    global _database
    with _lifecycle_lock:
        if _database is None:
            _database = Database(get_wing_home() / DB_FILENAME)
        return _database


def close_database() -> None:
    """关闭并丢弃单例（幂等；进程收尾调用）。

    先换单例、再关旧连接（`Database.close()` 自己会等它的在途事务结束）：关的过程
    不持生命周期锁——否则「事务里取单例」与「关闭方等事务」会互等成死锁。多语句的
    原子性不依赖单例是否还在：一次操作在构造时就把连接钉住了（见 `CacheStore`）。
    """
    global _database
    with _lifecycle_lock:
        database, _database = _database, None
    if database is not None:
        database.close()


def reset_database() -> None:
    """测试用：关掉单例，让下次 ``get_database()`` 按当前环境重新打开。

    与 :func:`close_database` 同一实现——区别只在意图（测试隔离 vs 进程收尾）。
    """
    close_database()
