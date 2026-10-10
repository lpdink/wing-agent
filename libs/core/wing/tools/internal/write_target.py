# wing/tools/internal/write_target.py
"""写入目标分类：把「就地写」（``open(path, "w")``）的可见行为对齐到原子替换之上。

原子替换（同目录 tmp + ``os.replace``）与就地写在几类目标上行为不同——本模块是
文件工具（Write / Edit）共用的**唯一**判定点：两工具用同一份政策，不再各写一半。
"""

from __future__ import annotations

import errno
import os
from pathlib import Path


def resolve_write_target(path: str) -> tuple[Path, bool]:
    """把调用方给的路径解析成 ``(写入目标, 是否就地写)``。

    - **符号链接**：解析到真实目标——原子替换会把链接本身换成普通文件，真实
      目标静默留着旧内容（写"去哪了"与模型的理解错位）；就地写时代是"穿过
      链接写真实目标"。链接目标处的目录**不**自动创建（时代语义：``open()``
      顺链接打开，父目录不存在即 ENOENT），否则会在链接指向的位置凭空建出
      目录树。
    - **FIFO / 设备节点等非常规文件**：就地写——rename 会把节点本身换掉，
      消费者（如 FIFO 读端）永远收不到字节。
    - **既有但不可写的常规文件**：``PermissionError``——rename 只需要目录写
      权限，目标自身的只读位拦不住替换；就地写时代的拒绝语义要显式补回。

    返回 ``in_place=True`` 时调用方应直接用 ``open(target, "w")`` 写（这些
    目标上原子替换会改变节点语义）；其余情况交给 ``atomic_write_text``。
    """
    target = Path(path)
    if target.is_symlink():
        target = Path(os.path.realpath(target))
        if not target.parent.exists():
            raise FileNotFoundError(
                errno.ENOENT, "No such file or directory", str(path)
            )

    if target.exists() and not target.is_file() and not target.is_dir():
        return target, True

    if target.is_file() and not os.access(target, os.W_OK):
        raise PermissionError(errno.EACCES, "Permission denied", str(path))

    return target, False
