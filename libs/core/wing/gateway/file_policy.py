# wing/gateway/file_policy.py — 文件服务策略（路径包含性 / 类型白名单 / 缓存头）

"""网关文件服务的**纯策略**层：路径判定零 I/O（`safe_stat` 是唯一例外，一处 syscall）。

两个使用者共用同一套判定：

- `routes/workspace.py`（受限图片端点）：会话 workspace 作为根，白名单是图片扩展名；
- `routes/static.py`（web 构建托管）：`gateway.static_dir` 作为根，类型表是静态资源表。

**安全不变量**：请求路径一律先 `realpath` 再与 `realpath(root)` 做**路径分量**
包含性比较（`Path.relative_to`）。禁止字符串前缀比较（`/ws-evil` 不能骗过 `/ws`），
也禁止"拼接后直接打开"（符号链接、`..`、绝对路径全部经同一条解析路径收敛）。

**文件系统错误**：`Path.is_file()` 只吞 ENOENT/ENOTDIR/EBADF/ELOOP，超长路径
（ENAMETOOLONG）会直接抛出——两处 `is_file()` 因此都换成本模块的
`safe_stat()` / `safe_is_file()`：任何 `OSError` 都收敛成"不是文件"（调用方回
404 / SPA fallback），绝不把 500 抛给客户端。
"""

from __future__ import annotations

import os
import stat as stat_module
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

#: 构建产物目录名（`/assets/*`，内容哈希命名）：可以承诺不可变。
ASSETS_DIRNAME = "assets"

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


def safe_stat(path: str | os.PathLike[str]) -> os.stat_result | None:
    """`os.stat` 的兜底形态：任何 `OSError` → None（不抛）。

    `Path.is_file()` 的吞噬名单只有 ENOENT / ENOTDIR / EBADF / ELOOP：超长路径
    （ENAMETOOLONG）、权限/IO 故障都会冒泡成 500。服务端对"取不到 stat"只有一种
    合理反应——当作没有这个文件（调用方映射 404 / fallback）。
    """
    try:
        return os.stat(path)
    except OSError:
        return None


def safe_file_stat(path: str | os.PathLike[str]) -> os.stat_result | None:
    """普通文件的 `stat`；不是普通文件（目录 / FIFO / 设备）或取不到 → None。

    两个路由都用它做"存在 + 是普通文件（+ 大小）"的一次性判定——静态路由还要把
    这份 `stat_result` 交给 `FileResponse`（省一次系统调用，并消除检查与 open 之间
    的竞态窗口）。
    """
    info = safe_stat(path)
    if info is None or not stat_module.S_ISREG(info.st_mode):
        return None
    return info


def safe_is_file(path: str | os.PathLike[str]) -> bool:
    """是否是普通文件（`safe_file_stat` 的布尔面）。

    符号链接按 `os.stat` 语义跟随（调用方传入的通常已是 realpath 结果）。
    """
    return safe_file_stat(path) is not None


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


def cache_control_for(
    target: str | os.PathLike[str], *, static_root: str | os.PathLike[str]
) -> str:
    """静态托管的缓存策略：**解析后**落在 `<root>/assets/**` 内 → 不可变；其余 no-cache。

    判定用解析后的真实路径，不是请求路径的字面首段：`/assets/%2e%2e%2findex.html`
    的真身是壳（每次部署都变），若按 URL 首段给 `immutable`，等于把"一年不变"的
    承诺挂到最不该缓存的文件上（反向代理一旦归一化 `%2e%2e`，后果与设计目标相反）。
    `root/assets` 自身也走 realpath，因此"assets 是指向别处的符号链接"这类布局与
    请求路径判定的结果一致。
    """
    assets_root = os.path.realpath(os.path.join(os.fspath(static_root), ASSETS_DIRNAME))
    resolved = os.path.realpath(os.fspath(target))
    if is_within(resolved, assets_root):
        return CACHE_CONTROL_IMMUTABLE
    return CACHE_CONTROL_NO_CACHE
