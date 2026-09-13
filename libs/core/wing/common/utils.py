import uuid
from datetime import datetime


# ============================================================
# Session ID 生成
# ============================================================


def generate_session_id() -> str:
    """生成唯一 session id：{YYYYMMDD-HHMMSS}-{8位uuid}。"""
    timestamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    short_uuid = str(uuid.uuid4()).replace("-", "")[:8]
    return f"{timestamp}-{short_uuid}"


# ============================================================
# 路径安全校验
# ============================================================


def _is_safe_path_component(name: str) -> bool:
    """校验字符串是否可作为安全的文件路径单层组件。

    拒绝空串、含路径分隔符、含父目录引用（..）的值，
    防止路径穿越攻击。
    """
    if not name:
        return False
    if "/" in name or "\\" in name or ".." in name:
        return False
    return True


# ============================================================
# 异常链格式化
# ============================================================


def _cheap_context(exc: BaseException) -> str:
    """尽力为异常补一点廉价上下文（鸭子类型，不依赖传输层）。

    只认 httpx 风格的 `.request`（`method` + `url`）——空消息异常多半来自
    传输层（如 `httpx.ReadError("")`），"哪个请求"是它唯一能说出的信息。

    `.request` 可能是个**会抛异常**的 property（httpx 在未绑定请求时抛
    RuntimeError）——取上下文属于尽力而为，绝不能因为它把格式化本身搞崩。
    """
    try:
        request = getattr(exc, "request", None)
        url = getattr(request, "url", None)
    except Exception:
        return ""
    if url is None:
        return ""
    method = str(getattr(request, "method", "") or "")
    return f"{method} {url}".strip()


def _describe_exception(exc: BaseException) -> str:
    """单个异常的一行描述：`Type: message`；消息为空时不留空尾。

    空消息（`ReadError: ` 这种）信息量为零，至少给类型名，并尽量补上下文。
    """
    name = type(exc).__name__
    message = str(exc).strip()
    if message:
        return f"{name}: {message}"
    context = _cheap_context(exc)
    return f"{name} ({context})" if context else name


def format_exception_chain(exc: BaseException, max_depth: int = 5) -> str:
    """格式化完整异常链：遍历 __cause__ 和 __context__，返回可读字符串。

    例如：APIConnectionError: Connection error. (caused by ConnectError: Connection refused)
    深层链：A: msg (caused by B: msg (caused by C: msg))

    空消息异常不会产出 `Type: ` 这样的空尾文案（见 `_describe_exception`）。
    """
    parts: list[str] = []
    seen: set[int] = set()
    current: BaseException | None = exc

    while current is not None and len(parts) < max_depth:
        exc_id = id(current)
        if exc_id in seen:
            break
        seen.add(exc_id)

        parts.append(_describe_exception(current))

        current = current.__cause__ or current.__context__

    if not parts:
        return str(exc)

    result = parts[0]
    for cause in parts[1:]:
        result += f" (caused by {cause})"
    return result
