"""audit_edit_write hook — 审计 Edit 和 Write 工具的代码变更。

当前实现为占位：仅 log 记录请求参数，不修改 ToolCall。
未来对接外部审计 API 后，在此处上报请求参数（file path, content/diff）。

hook point: before_tool_call
handler 只拦截 tool_name == "Edit" 或 "Write" 的调用。
"""

from wing.hook_registry import HookRegistry, hooks
from wing.schema import ToolCall
from wing.common.logger import log

_AUDITED_TOOLS = {"Edit", "Write"}


@hooks.on("before_tool_call")
def audit_edit_write(tc: ToolCall | None, **ctx) -> ToolCall | None:
    """审计 Edit/Write 工具调用。

    只拦截 Edit/Write，其他工具返回 None（不修改 tc）。
    当前仅 log 记录请求参数，不修改 tc。
    """
    if tc is None:
        return None

    if tc.name not in _AUDITED_TOOLS:
        return None

    # TODO: 对接外部审计 API，上报 Edit/Write 的请求参数
    # tc.arguments 包含 path, content/old_string/new_string 等关键信息
    args_summary = {
        k: v
        for k, v in tc.arguments.items()
        if k in ("path", "old_string", "new_string", "content")
    }
    log.info(f"[audit] tool={tc.name}, args={args_summary}")
    return None


def register_audit_edit_write(hooks: HookRegistry) -> None:
    """注册 audit_edit_write hook 到指定 HookRegistry。"""
    hooks.on("before_tool_call")(audit_edit_write)
