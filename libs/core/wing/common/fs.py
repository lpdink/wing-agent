"""
wing/common/fs.py — 文件系统原子写工具。

统一的 tmp + fsync + rename 原子写语义，供所有需要落盘的模块复用。
"""

from __future__ import annotations

import json
import os
import threading
from pathlib import Path
from typing import Any


def _tmp_path(path: Path) -> Path:
    """落盘临时文件名：pid + 线程 id 双层唯一。

    tmp 名一旦只按 pid 唯一，同进程多线程并发写同一路径就会撞名：两个线程
    打开同一个 tmp，先到者 os.replace 把它改名走，后到者 os.replace 找不到
    源文件抛 FileNotFoundError（review r1 S1，实测 3/4 线程必现）。存活的
    线程 id 互不相同，且本模块是同步函数（同线程内不存在交错），故 pid +
    tid 组合足以覆盖进程内并发；跨进程由 pid 区分。

    不用 tempfile.mkstemp：它把临时文件建成 0600，os.replace 后最终文件会
    继承该权限，与既有落盘语义（open 受 umask 影响，通常 0644）不一致。
    """
    return path.with_name(f"{path.name}.tmp.{os.getpid()}.{threading.get_ident()}")


def _discard_tmp(tmp: Path) -> None:
    """删除失败的 tmp 文件（尽力而为；删除失败不掩盖原始异常）。"""
    try:
        tmp.unlink()
    except OSError:
        pass


def atomic_write_text(path: Path, text: str) -> None:
    """原子写入文本文件：tmp + fsync + replace。自动创建父目录。

    使用 os.replace 而非 os.rename：目标存在时原子替换（Windows 上
    os.rename 会因目标存在而失败，而 metadata.json 等文件会被反复覆盖）。
    失败时丢弃 tmp、目标原样不动——要么整体可见、要么什么都没发生，不会在
    目标目录留下 `.tmp.<pid>.<tid>` 垃圾（与 Edit 的失败语义一致）。
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = _tmp_path(path)
    try:
        with open(tmp, "w", encoding="utf-8") as f:
            f.write(text)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
    except BaseException:
        _discard_tmp(tmp)
        raise


def atomic_write_bytes(path: Path, data: bytes) -> None:
    """原子写入二进制文件：tmp + fsync + replace。自动创建父目录。

    与 atomic_write_text 同一语义，供媒体字节等二进制载荷复用（内容寻址
    写入必须要么完整可见、要么不可见，不能留下半截文件被当成有效对象）。
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = _tmp_path(path)
    try:
        with open(tmp, "wb") as f:
            f.write(data)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
    except BaseException:
        _discard_tmp(tmp)
        raise


def atomic_write_json(path: Path, data: Any, *, indent: int | None = None) -> None:
    """原子写入 JSON 文件：tmp + fsync + rename。自动创建父目录。"""
    atomic_write_text(path, json.dumps(data, ensure_ascii=False, indent=indent))
