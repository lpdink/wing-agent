# wing/gateway/file_policy.py — 文件服务策略（路径包含性 / 类型白名单 / 缓存头）

"""网关文件服务的**纯策略**层：零 I/O、零全局状态，全部可单测。

两个使用者共用同一套判定：

- `routes/workspace.py`（受限图片端点）：会话 workspace 作为根，白名单是图片扩展名；
- `routes/static.py`（web 构建托管）：`gateway.static_dir` 作为根，类型表是静态资源表。

**安全不变量**：请求路径一律先 `realpath` 再与 `realpath(root)` 做**路径分量**
包含性比较（`Path.relative_to`）。禁止字符串前缀比较（`/ws-evil` 不能骗过 `/ws`），
也禁止"拼接后直接打开"（符号链接、`..`、绝对路径全部经同一条解析路径收敛）。
"""

from __future__ import annotations

import os
from pathlib import Path, PurePath
from typing import Mapping

# ── 图片端点（接口冻结：白名单 / 上限 / Content-Type） ──────────────

#: 单文件字节上限（10 MiB）：上限本身允许，超过即 413。
IMAGE_MAX_BYTES = 10 * 1024 * 1024

#: 图片扩展名白名单（与 `extensions/vscode/src/host/images.ts` 逐项同口径）。
IMAGE_EXTENSIONS: frozenset[str] = frozenset(
    {".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".bmp", ".ico", ".avif", ".apng"}
)

#: 扩展名 → Content-Type（图片端点；100% 覆盖 IMAGE_EXTENSIONS，由单测钉住）。
IMAGE_CONTENT_TYPES: Mapping[str, str] = {
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".gif": "image/gif",
    ".webp": "image/webp",
    ".svg": "image/svg+xml",
    ".bmp": "image/bmp",
    ".ico": "image/x-icon",
    ".avif": "image/avif",
    ".apng": "image/apng",
}

# ── 静态托管（Content-Type / 缓存头） ─────────────────────────────

#: 静态资源的 Content-Type。显式表（不查平台 mimetypes 数据库）：判定确定性
#: 是断言前提，`mimetypes` 在不同机器/系统上会漂（且对 avif/apng/webmanifest 无能）。
STATIC_CONTENT_TYPES: Mapping[str, str] = {
    ".html": "text/html; charset=utf-8",
    ".htm": "text/html; charset=utf-8",
    ".js": "text/javascript; charset=utf-8",
    ".mjs": "text/javascript; charset=utf-8",
    ".cjs": "text/javascript; charset=utf-8",
    ".css": "text/css; charset=utf-8",
    ".json": "application/json",
    ".map": "application/json",
    ".webmanifest": "application/manifest+json",
    ".txt": "text/plain; charset=utf-8",
    ".xml": "application/xml",
    ".svg": "image/svg+xml",
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".gif": "image/gif",
    ".webp": "image/webp",
    ".avif": "image/avif",
    ".apng": "image/apng",
    ".bmp": "image/bmp",
    ".ico": "image/x-icon",
    ".woff": "font/woff",
    ".woff2": "font/woff2",
    ".ttf": "font/ttf",
    ".otf": "font/otf",
    ".wasm": "application/wasm",
    ".pdf": "application/pdf",
}

#: 未知扩展名的兜底（浏览器会按嗅探规则处理，静态托管不额外限制）。
DEFAULT_STATIC_CONTENT_TYPE = "application/octet-stream"

#: 构建产物（内容哈希命名）：可以承诺不可变。
CACHE_CONTROL_IMMUTABLE = "public, max-age=31536000, immutable"

#: 其余一切（含 index.html）：每次带条件请求重验证，绝不拿旧壳。
CACHE_CONTROL_NO_CACHE = "no-cache"

#: 图片端点：同一路径的内容可能被重写（读图后覆盖同名文件即换图），不缓存。
CACHE_CONTROL_IMAGE = "no-store"


class FilePolicyError(Exception):
    """路径策略拒绝（携带面向客户端的简短理由）。"""


class BadPathError(FilePolicyError):
    """请求路径本身不可用（空 / NUL）→ 调用方映射 400。"""


class PathOutsideRootError(FilePolicyError):
    """解析后落在根之外（含符号链接逃逸）→ 调用方映射 403。"""


def is_within(path: str | os.PathLike[str], root: str | os.PathLike[str]) -> bool:
    """`path` 是否在 `root` 内（**按路径分量**比较，不是字符串前缀）。

    两侧先 `normpath`（折叠 `..` / 重复斜杠 / 尾斜杠）再比较：调用方通常已经
    realpath 过，这里是"万一没归一"的兜底——字符串前缀比较会把 `/ws-evil`
    判成 `/ws` 的子路径。
    """
    normalized = os.path.normpath(os.fspath(path))
    normalized_root = os.path.normpath(os.fspath(root))
    try:
        PurePath(normalized).relative_to(PurePath(normalized_root))
    except ValueError:
        return False
    return True


def resolve_within_root(root: str | os.PathLike[str], raw: str) -> Path:
    """把请求路径解析为根内的**真实**路径（已 realpath）。

    - 相对路径按 `root` 解析；绝对路径原样进入包含性校验（必须落在根内）；
    - `~` **不展开**（请求路径不解释用户 home；`~` 只是普通目录名）；
    - 符号链接、`..`、重复斜杠全部由 `os.path.realpath` 归一后再比较。

    Raises:
        BadPathError: `raw` 为空或含 NUL 字节。
        PathOutsideRootError: 解析结果不在 `root` 内。
    """
    if not raw or "\x00" in raw:
        raise BadPathError("path is empty or contains a NUL byte")
    root_real = os.path.realpath(os.path.expanduser(os.fspath(root)))
    candidate = raw if os.path.isabs(raw) else os.path.join(root_real, raw)
    resolved = os.path.realpath(candidate)
    if not is_within(resolved, root_real):
        raise PathOutsideRootError("path escapes the allowed root")
    return Path(resolved)


def _suffix(path: str | os.PathLike[str]) -> str:
    """小写扩展名（`Path.suffix` 的形态，含点；无扩展名为空串）。"""
    return PurePath(os.fspath(path)).suffix.lower()


def image_content_type(path: str | os.PathLike[str]) -> str | None:
    """图片端点的 Content-Type；扩展名不在白名单时返回 None（调用方 404）。"""
    return IMAGE_CONTENT_TYPES.get(_suffix(path))


def static_content_type(path: str | os.PathLike[str]) -> str:
    """静态托管的 Content-Type（未知扩展名兜底 `application/octet-stream`）。"""
    return STATIC_CONTENT_TYPES.get(_suffix(path), DEFAULT_STATIC_CONTENT_TYPE)


def cache_control_for(relative_path: str) -> str:
    """静态托管的缓存策略：`/assets/*` 不可变；其余（含 index.html）no-cache。

    判定用**请求路径**（URL 上的相对路径）而不是 realpath 结果：缓存策略属于
    「这个 URL 承诺了哪种新鲜度」，与文件在磁盘上的落点无关。
    """
    parts = PurePath(relative_path.lstrip("/")).parts
    if parts and parts[0] == "assets":
        return CACHE_CONTROL_IMMUTABLE
    return CACHE_CONTROL_NO_CACHE
