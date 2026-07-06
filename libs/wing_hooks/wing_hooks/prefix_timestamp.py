"""prefix_timestamp hook — 在 user message 之前加上当前时间戳。

格式：[YYYY-MM-DD HH:MM:SS] original_content
"""

from datetime import datetime

from wing.hook_registry import HookRegistry, hooks


@hooks.on("before_user_message")
def prefix_timestamp(content: str | None, **ctx) -> str | None:
    """给用户消息前缀时间戳。

    None 或空字符串 → 返回 None（不修改）
    正常字符串 → 前缀 [YYYY-MM-DD HH:MM:SS]
    """
    if not content:
        return None
    time_str = f"[{datetime.now().strftime('%Y-%m-%d %H:%M:%S')}]"
    return " ".join([time_str, content])


def register_prefix_timestamp(hooks: HookRegistry) -> None:
    """注册 prefix_timestamp hook 到指定 HookRegistry。"""
    hooks.on("before_user_message")(prefix_timestamp)
