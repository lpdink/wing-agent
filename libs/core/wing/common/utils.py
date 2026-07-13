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


def format_exception_chain(exc: BaseException, max_depth: int = 5) -> str:
    """格式化完整异常链：遍历 __cause__ 和 __context__，返回可读字符串。

    例如：APIConnectionError: Connection error. (caused by ConnectError: Connection refused)
    深层链：A: msg (caused by B: msg (caused by C: msg))
    """
    parts: list[str] = []
    seen: set[int] = set()
    current: BaseException | None = exc

    while current is not None and len(parts) < max_depth:
        exc_id = id(current)
        if exc_id in seen:
            break
        seen.add(exc_id)

        label = f"{type(current).__name__}: {current}"
        parts.append(label)

        current = current.__cause__ or current.__context__

    if not parts:
        return str(exc)

    result = parts[0]
    for cause in parts[1:]:
        result += f" (caused by {cause})"
    return result
