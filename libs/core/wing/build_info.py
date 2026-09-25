# wing/build_info.py
"""网关构建信息（版本 + commit hash）。

构建/安装时由 ``libs/core/hatch_build.py`` 注入 ``wing/_build_info.py``；
本模块是运行期唯一的读取口——只读生成文件，不做任何 git 调用。

取值在模块导入时定型（即进程启动快照）：生成文件即使被后续重装刷新，
已启动的进程仍报告启动时那份构建信息。生成文件缺失（未经构建的源码树）
时返回 None，由调用方决定兜底展示。
"""

from __future__ import annotations


def _load_build_info() -> tuple[str | None, str | None]:
    """读取生成文件，返回 (version, commit)；生成文件缺失时返回 (None, None)。"""
    try:
        from wing._build_info import commit, version
    except ImportError:  # 未经构建的源码树（无生成文件）
        return None, None
    return version, commit


_version, _commit = _load_build_info()


def get_version() -> str | None:
    """构建时注入的包版本（hatch-vcs 从 git tag 推导）；不可用时返回 None。"""
    return _version


def get_commit() -> str | None:
    """构建时注入的 commit hash（短）；不可用时返回 None。"""
    return _commit
