# wing/tools/internal/io_errors.py
"""文件类工具的文件系统错误原因串。

原因串只有四种：``No such file`` / ``Permission denied`` / ``Is a directory``
（三种「下一步能做什么」各不相同的失败）与 ``strerror``（其余 errno 原样带出内核文案）。

取用点：``read`` 的 stat 段、``ReadImage`` 的 stat 与 open 段、``edit`` 的缺文件分支、
``write`` 的权限 / 目录两个分支。（``read`` 的 open 段用 ``{e}`` 而不是原因串——它
刻意把 errno 前缀一起带给模型；``read`` / ``ReadImage`` 的目录判断是在 stat **成功**
之后做的，直接写文案。）

**模板与「捕获哪些异常」仍在各工具自己手里**：``read`` 是 ``read_file: {path}: …``、
``ReadImage`` 是 ``ReadImage: {path}: …``、``edit`` 是 ``edit: {path}: …``、
``write`` 是 ``write: cannot write '{path}': …``；哪些 ``OSError`` 折成 ``ToolError``、
哪些照旧逃逸也各不相同（``read`` 的 stat 段只接两个子类，``read_image`` 接整个
``OSError``，``edit`` 只接三个，``write`` 四种各一套）。那些差异是模型可见的错误面，
逐处保留。
"""

from __future__ import annotations


def reason_of(exc: OSError) -> str:
    """异常 → 原因串：三个固定串优先，其余取 ``strerror``（缺失回落 ``str(exc)``）。"""
    if isinstance(exc, FileNotFoundError):
        return "No such file"
    if isinstance(exc, PermissionError):
        return "Permission denied"
    if isinstance(exc, IsADirectoryError):
        return "Is a directory"
    return exc.strerror or str(exc)


__all__ = ["reason_of"]
