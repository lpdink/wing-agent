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
