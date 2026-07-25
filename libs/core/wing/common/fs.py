"""
wing/common/fs.py — 文件系统原子写工具。

统一的 tmp + fsync + rename 原子写语义，供所有需要落盘的模块复用。
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any


def atomic_write_text(path: Path, text: str) -> None:
    """原子写入文本文件：tmp + fsync + rename。自动创建父目录。"""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(f".tmp.{os.getpid()}")
    with open(tmp, "w", encoding="utf-8") as f:
        f.write(text)
        f.flush()
        os.fsync(f.fileno())
    os.rename(tmp, path)


def atomic_write_json(path: Path, data: Any, *, indent: int | None = None) -> None:
    """原子写入 JSON 文件：tmp + fsync + rename。自动创建父目录。"""
    atomic_write_text(path, json.dumps(data, ensure_ascii=False, indent=indent))
