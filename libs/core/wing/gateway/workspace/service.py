# wing/gateway/workspace/service.py — 只读文件系统访问（本地 workspace provider）

"""Workspace 文件浏览的服务层：宿主文件系统的只读访问。

语义（与协议面的约定）：

- 路径是**提供方命名空间的绝对路径**——本实现即网关宿主机的文件系统。相对路径
  一律拒绝（那是网关进程 cwd 这类本地细节，远程实现里没有对应物）。
- **只读**：全部操作是 ``scandir`` / ``lstat`` / ``readlink`` / 常规文件的 read；
  没有写 / 上传 / 删除路径（写继续只走 agent）。
- **符号链接**：列举与 stat 用 ``lstat`` 口径（链接本身是 ``symlink`` 条目，附
  ``symlink_target`` / ``broken``）；读取跟随链接一次（由 OS 解析）。
- **只读常规文件**：目录 / FIFO / 设备一律拒绝读取——FIFO 会挂住读线程，
  设备是无界流，都不是「文件的字节」。
- 错误面唯一出口是 :class:`WorkspaceError`（HTTP 状态 + 文案），路由不做二次分类。

将来远程工具宿主以同一形状暴露「它所在环境的文件系统」时，替换的就是本模块。
"""

from __future__ import annotations

import mimetypes
import os
import stat
from dataclasses import dataclass

from .protocol import EntryType

#: 目录列举的默认 / 上限条数（响应限幅；``total`` 仍反映扫描到的全量）。
DEFAULT_LIST_LIMIT = 200
MAX_LIST_LIMIT = 1000


@dataclass(frozen=True, slots=True)
class Entry:
    """一个文件系统条目（列举项 / stat 结果共用的纯数据形状）。"""

    name: str
    path: str
    type: EntryType
    size: int | None
    """字节数（仅常规文件；目录 / 链接 / 其它为 ``None``——目录的 st_size 是文件系统内部值）。"""
    mtime: float
    """``lstat`` 的修改时间（epoch 秒）。"""
    mime: str | None
    """按扩展名猜的 MIME（仅常规文件；猜不出为 ``None``）。"""
    symlink_target: str | None
    """链接原文（仅 ``symlink``；读取失败为 ``None``）。"""
    broken: bool
    """链接目标不可解析（缺失 / 环 / 不可访问；仅 ``symlink`` 可能为真）。"""


@dataclass(frozen=True, slots=True)
class Listing:
    """一次目录列举的结果（``entries`` 是 ``[offset, offset+limit)`` 的窗口）。"""

    path: str
    entries: tuple[Entry, ...]
    total: int
    """有效条目总数（不可表示的条目已跳过；竞态消失的不计）。"""
    next_offset: int | None
    """下一页偏移；``None`` = 已到末尾。"""


@dataclass(frozen=True, slots=True)
class ReadTarget:
    """一次可读文件的前置校验结果（交给 ``FileResponse`` 的素材）。"""

    path: str
    name: str
    mime: str | None
    """按扩展名猜的 MIME（猜不出为 ``None``，路由据此决定安全头与线类型）。"""
    stat: os.stat_result


@dataclass(frozen=True, slots=True)
class Root:
    """一个可浏览的起点。"""

    path: str
    kind: str
    """``home`` / ``cwd``。"""
    name: str


class WorkspaceError(Exception):
    """客户端可见的错误——服务层唯一的失败出口，路由原样映射为统一错误形状。"""

    def __init__(self, status: int, detail: str) -> None:
        super().__init__(detail)
        self.status = status
        self.detail = detail


# ─────────────────────────────────────────────────────────────────────────────
# 路径
# ─────────────────────────────────────────────────────────────────────────────


def normalize_path(raw: str) -> str:
    """绝对路径 + 词法归一（不触盘、不解析符号链接；``..`` 只是普通导航）。

    ``os.path.abspath`` 收敛 ``.`` / ``..`` / 重复分隔符，但**不**解析符号链接——
    响应里回显的路径因此与请求者给的路径保持同一命名（不悄悄换成链接目标）。
    """
    if "\x00" in raw:
        raise WorkspaceError(400, "path contains a NUL byte")
    if not os.path.isabs(raw):
        raise WorkspaceError(400, f"path must be absolute: {raw!r}")
    return os.path.abspath(raw)


def _wire_text(value: str) -> str | None:
    """JSON 线格式是 UTF-8：不可编码的值不上线（Linux 文件名可以是任意字节）。"""
    try:
        value.encode("utf-8")
    except UnicodeEncodeError:
        return None
    return value


# ─────────────────────────────────────────────────────────────────────────────
# 条目
# ─────────────────────────────────────────────────────────────────────────────


def _symlink_target(path: str) -> str | None:
    try:
        return os.readlink(path)
    except OSError:
        return None


def _guess_mime(name: str) -> str | None:
    return mimetypes.guess_type(name)[0]


def _describe(name: str, path: str, st: os.stat_result) -> Entry:
    """``lstat`` 结果 → 条目的公共形状（口径见模块 docstring）。"""
    mode = st.st_mode
    if stat.S_ISLNK(mode):
        target = _symlink_target(path)
        return Entry(
            name=name,
            path=path,
            type="symlink",
            size=None,
            mtime=st.st_mtime,
            mime=None,
            symlink_target=None if target is None else _wire_text(target),
            broken=not os.path.exists(path),
        )
    if stat.S_ISDIR(mode):
        return Entry(
            name=name,
            path=path,
            type="dir",
            size=None,
            mtime=st.st_mtime,
            mime=None,
            symlink_target=None,
            broken=False,
        )
    if stat.S_ISREG(mode):
        return Entry(
            name=name,
            path=path,
            type="file",
            size=st.st_size,
            mtime=st.st_mtime,
            mime=_guess_mime(name),
            symlink_target=None,
            broken=False,
        )
    return Entry(
        name=name,
        path=path,
        type="other",
        size=None,
        mtime=st.st_mtime,
        mime=None,
        symlink_target=None,
        broken=False,
    )


def _sort_key(entry: Entry) -> tuple[int, str, str]:
    """目录在前；组内按 ``name.casefold()`` 再按 ``name``——全序，翻页稳定。"""
    return (0 if entry.type == "dir" else 1, entry.name.casefold(), entry.name)


# ─────────────────────────────────────────────────────────────────────────────
# 浏览
# ─────────────────────────────────────────────────────────────────────────────


def roots() -> tuple[Root, ...]:
    """可浏览的起点：home 与网关进程 cwd（去重、只保留真实存在的目录）。

    「工作区选择」的入口：前端从这些起点出发浏览，选中目录后再交给会话的
    ``workspace`` 字段。没有配置——起点是宿主事实，不引入第二个真相。
    """
    out: list[Root] = []
    seen: set[str] = set()
    candidates = (("home", os.path.expanduser("~")), ("cwd", _process_cwd()))
    for kind, raw in candidates:
        if raw is None or raw.startswith("~") or raw == os.sep:
            # ``HOME=""`` 时 ``expanduser("~")`` 回落到 ``/``——根目录不是「家目录」
            # （前端会把它当起点入口），宁可不给。
            continue
        try:
            path = os.path.abspath(raw)
        except OSError:
            continue
        if path in seen or not os.path.isdir(path):
            continue
        seen.add(path)
        out.append(Root(path=path, kind=kind, name=os.path.basename(path) or path))
    return tuple(out)


def _process_cwd() -> str | None:
    try:
        return os.getcwd()
    except OSError:  # cwd 已被删除（进程活着，目录没了）
        return None


def list_dir(raw: str, *, offset: int = 0, limit: int = DEFAULT_LIST_LIMIT) -> Listing:
    """列举目录（分页窗口）。

    ``total`` 是扫描到的全量有效条目数；``entries`` 是排序后切出的窗口。
    竞态（扫描后、``lstat`` 前条目消失）即跳过——不把半截数据当条目。
    """
    path = normalize_path(raw)
    try:
        with os.scandir(path) as it:
            scanned = list(it)
    except FileNotFoundError as exc:
        raise WorkspaceError(404, f"directory not found: {path}") from exc
    except NotADirectoryError as exc:
        raise WorkspaceError(400, f"not a directory: {path}") from exc
    except PermissionError as exc:
        raise WorkspaceError(403, f"permission denied: {path}") from exc
    except OSError as exc:
        raise WorkspaceError(400, f"cannot list directory: {path}") from exc

    entries: list[Entry] = []
    for item in scanned:
        if _wire_text(item.name) is None:
            continue
        try:
            st = os.lstat(item.path)
        except OSError:
            continue
        entries.append(_describe(item.name, item.path, st))

    entries.sort(key=_sort_key)
    total = len(entries)
    window = entries[offset : offset + limit]
    next_offset = offset + limit if offset + limit < total else None
    return Listing(
        path=path, entries=tuple(window), total=total, next_offset=next_offset
    )


def stat_path(raw: str) -> Entry:
    """单个条目的元数据（``lstat`` 口径，不跟随符号链接）。"""
    path = normalize_path(raw)
    try:
        st = os.lstat(path)
    except FileNotFoundError as exc:
        raise WorkspaceError(404, f"no such entry: {path}") from exc
    except PermissionError as exc:
        raise WorkspaceError(403, f"permission denied: {path}") from exc
    except OSError as exc:
        raise WorkspaceError(400, f"cannot stat: {path}") from exc
    return _describe(os.path.basename(path) or path, path, st)


def resolve_read(raw: str) -> ReadTarget:
    """读取的前置校验：只允许**常规文件**（符号链接跟随一次）。

    校验方式是**真正 open 一次**（``O_RDONLY | O_NONBLOCK``，随即关闭）：权限错误在
    这里就变成 403，而不是流式响应发出 ``200`` 后再中途断掉（长度撒谎）；``O_NONBLOCK``
    让 FIFO 的 open 立即返回（不挂住调用线程），类型由 ``fstat`` 判定。

    Raises:
        WorkspaceError: 不存在（404）/ 不可访问（403）/ 目录或非常规文件（400）。
    """
    path = normalize_path(raw)
    try:
        fd = os.open(path, os.O_RDONLY | getattr(os, "O_NONBLOCK", 0))
    except FileNotFoundError as exc:
        raise WorkspaceError(404, f"no such entry: {path}") from exc
    except PermissionError as exc:
        raise WorkspaceError(403, f"permission denied: {path}") from exc
    except OSError as exc:
        raise WorkspaceError(400, f"cannot read: {path}") from exc
    try:
        st = os.fstat(fd)
    finally:
        os.close(fd)
    if stat.S_ISDIR(st.st_mode):
        raise WorkspaceError(400, f"is a directory: {path}")
    if not stat.S_ISREG(st.st_mode):
        raise WorkspaceError(400, f"not a regular file: {path}")
    name = os.path.basename(path) or "download"
    return ReadTarget(path=path, name=name, mime=_guess_mime(name), stat=st)
