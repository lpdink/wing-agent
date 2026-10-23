"""``wing.db`` 的只读断言口。

场景要断言「落盘事实」时不该经产品读路径（那是在用被测代码验证被测代码），而是
直接读同一个库文件。两条纪律：

- **只读连接**（``mode=ro``）：库里没有这张表时明确报错，而不是悄悄建一个空库；
- **不要写**：场景用 ``probe.env.wing_home / "core" / "wing.db"`` 定位文件。

    conn = read_only_connect(probe.env.wing_home / "core" / "wing.db")
    try:
        rows = conn.execute("SELECT * FROM audit_llm_call").fetchall()
    finally:
        conn.close()
"""

from __future__ import annotations

import sqlite3
from pathlib import Path

__all__ = ["read_only_connect"]


def read_only_connect(path: Path) -> sqlite3.Connection:
    """只读连接（``row_factory`` 已设为 ``sqlite3.Row``：按列名取值）。"""
    conn = sqlite3.connect(f"{path.as_uri()}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    return conn
