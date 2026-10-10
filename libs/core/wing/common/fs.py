"""
wing/common/fs.py — 文件系统原子写工具。

统一的 tmp + fsync + rename 原子写语义，供所有需要落盘的模块复用。
"""

from __future__ import annotations

import errno
import json
import os
import stat
import threading
from collections.abc import Callable
from pathlib import Path
from typing import Any


def _tmp_path(path: Path) -> Path:
    """落盘临时文件名：pid + 线程 id 双层唯一。

    tmp 名一旦只按 pid 唯一，同进程多线程并发写同一路径就会撞名：两个线程
    打开同一个 tmp，先到者 os.replace 把它改名走，后到者 os.replace 找不到
    源文件抛 FileNotFoundError。存活的
    线程 id 互不相同，且本模块是同步函数（同线程内不存在交错），故 pid +
    tid 组合足以覆盖进程内并发；跨进程由 pid 区分。

    tmp 的**创建权限**按目标分档（见 `_write_atomically`）：覆盖既有文件时
    先按 0600 建（内容刚落盘、chmod 之前不放开给其他用户），最终落到目标的
    权限位；新建按 0666 建，受 umask 影响——与就地写（`open(path, "w")`）
    的落地权限一致。
    """
    return path.with_name(f"{path.name}.tmp.{os.getpid()}.{threading.get_ident()}")


def _target_mode(path: Path) -> int | None:
    """既有目标的权限位（stat 失败 = 新建 / 不可读，返回 None）。

    就地写保留既有 inode 的权限位，而 tmp + replace 落地的是一份新 inode：
    不显式保留的话，0755 脚本 / 0600 密钥文件会被静默改写成 umask 默认值
    （可执行位丢失、`.env` 变 0644）。
    """
    try:
        return stat.S_IMODE(path.stat().st_mode)
    except OSError:
        return None


def _discard_tmp(tmp: Path) -> None:
    """删除失败的 tmp 文件（尽力而为；删除失败不掩盖原始异常）。"""
    try:
        tmp.unlink()
    except OSError:
        pass


def _write_atomically(
    path: Path, write: Callable[[Any], None], *, binary: bool
) -> None:
    """tmp + fsync + replace 的公共骨架（文本 / 字节两个入口共用）。

    - 覆盖既有文件：tmp 先按 0600 建、写完 chmod 回目标权限位再 replace
      （窗口期与最终落地权限都跟随目标）；
    - 新建：tmp 按 0666 建，受 umask 影响——与就地写的落地权限一致；
    - 失败（写 / swap 任一步）丢弃 tmp，目标原样不动——要么整体可见、要么
      什么都没发生，不在目标目录留下 `.tmp.<pid>.<tid>` 垃圾。
    """
    if path.parent == path:
        # 文件系统根（"/"、"."）：with_name 会抛 ValueError("empty name")——
        # 就地写是 Is a directory，保住这个分类（否则实现细节泄漏进诊断）。
        raise IsADirectoryError(errno.EISDIR, "Is a directory", str(path))

    try:
        # 父路径组件是文件（而非目录）时，mkdir 报 EEXIST——转成就地写同款
        # 的 ENOTDIR（"Not a directory"），模型看到的是能自纠的诊断。
        path.parent.mkdir(parents=True, exist_ok=True)
    except FileExistsError as exc:
        raise NotADirectoryError(errno.ENOTDIR, "Not a directory", str(path)) from exc

    tmp = _tmp_path(path)
    mode = _target_mode(path)
    try:
        # O_NOFOLLOW：tmp 名可预测（pid + tid），目标目录里若被预置同名符号
        # 链接，裸 open 会写穿到链接指向的文件并把链接装到目标路径上——拒绝
        # 打开（失败清理会删掉它）。陈旧普通 tmp 文件仍被 O_TRUNC 复用。
        # Windows 无该标志（getattr 兜底 0，那里也建不出符号链接的 tmp）。
        fd = os.open(
            tmp,
            os.O_CREAT | os.O_WRONLY | os.O_TRUNC | getattr(os, "O_NOFOLLOW", 0),
            0o600 if mode is not None else 0o666,
        )
        handle = os.fdopen(fd, "wb") if binary else os.fdopen(fd, "w", encoding="utf-8")
        with handle as f:
            write(f)
            f.flush()
            os.fsync(f.fileno())
        if mode is not None:
            os.chmod(tmp, mode)
        os.replace(tmp, path)
    except BaseException:
        _discard_tmp(tmp)
        raise


def atomic_write_text(path: Path, text: str) -> None:
    """原子写入文本文件：tmp + fsync + replace。自动创建父目录。

    使用 os.replace 而非 os.rename：目标存在时原子替换（Windows 上
    os.rename 会因目标存在而失败，而 metadata.json 等文件会被反复覆盖）。
    覆盖既有文件时保留其权限位；失败时丢弃 tmp、目标原样不动。
    """
    _write_atomically(path, lambda f: f.write(text), binary=False)


def atomic_write_bytes(path: Path, data: bytes) -> None:
    """原子写入二进制文件：tmp + fsync + replace。自动创建父目录。

    与 atomic_write_text 同一语义，供媒体字节等二进制载荷复用（内容寻址
    写入必须要么完整可见、要么不可见，不能留下半截文件被当成有效对象）。
    """
    _write_atomically(path, lambda f: f.write(data), binary=True)


def atomic_write_json(path: Path, data: Any, *, indent: int | None = None) -> None:
    """原子写入 JSON 文件：tmp + fsync + rename。自动创建父目录。"""
    atomic_write_text(path, json.dumps(data, ensure_ascii=False, indent=indent))
