# wing/gateway/workspace/protocol.py — Workspace 文件浏览的 wire 模型

"""``/api/workspace/*`` 的请求 / 响应模型——字段即线格式。

本子包自含协议（不复用 ``gateway/protocol`` 的包根出口）：将来「拆 Gateway」时
整个子包原样带走。
"""

from __future__ import annotations

from pydantic import BaseModel, Field

from .service import EntryType


class WorkspaceEntry(BaseModel):
    """文件系统条目（list 元素与 stat 响应共用同一形状）。"""

    name: str = Field(description="条目名（不含路径）")
    path: str = Field(description="条目绝对路径（词法归一；不解析符号链接）")
    type: EntryType = Field(description="file / dir / symlink / other（lstat 口径）")
    size: int | None = Field(
        default=None, description="字节数（仅常规文件；目录 / 链接 / 其它为 null）"
    )
    mtime: float = Field(description="lstat 的修改时间（epoch 秒）")
    mime: str | None = Field(
        default=None, description="按扩展名猜的 MIME（仅常规文件；猜不出为 null）"
    )
    symlink_target: str | None = Field(
        default=None,
        description="链接原文（仅 symlink；读取失败或不可编码为 UTF-8 为 null）",
    )
    broken: bool = Field(
        default=False, description="symlink 的目标不可解析（缺失 / 环 / 不可访问）"
    )


class WorkspaceListResponse(BaseModel):
    """``GET /api/workspace/list`` 响应——一个目录的条目窗口。"""

    path: str = Field(description="被列举目录的绝对路径（词法归一后的回显）")
    entries: list[WorkspaceEntry] = Field(description="当前窗口的条目（目录在前）")
    total: int = Field(description="有效条目总数（与窗口无关）")
    next_offset: int | None = Field(
        default=None, description="下一页 offset；null = 已到末尾"
    )


class WorkspaceRoot(BaseModel):
    """一个可浏览的起点（工作区选择的入口）。"""

    path: str = Field(description="起点绝对路径")
    kind: str = Field(description="home / cwd")
    name: str = Field(description="展示名（目录名）")


class WorkspaceRootsResponse(BaseModel):
    """``GET /api/workspace/roots`` 响应——可浏览的起点列表。"""

    roots: list[WorkspaceRoot] = Field(description="起点（去重、只含存在的目录）")
