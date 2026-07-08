"""truncate_tool_result hook — 截断过长的 tool result。

超过 MAX_TRUNCATE_LENGTH (20000) 时，前后各保留 KEEP_HEAD_LENGTH (2000)
和 KEEP_TAIL_LENGTH (2000) 字符，中间截断标记包含原始长度和临时文件路径。
原始完整结果写入持久化临时文件，agent 可通过 Read 工具查看。

对所有 tool_name 都生效（不限于 Bash）。
"""

import tempfile
from pathlib import Path

from wing.hook_registry import HookRegistry, hooks

MAX_TRUNCATE_LENGTH = 50000
KEEP_HEAD_LENGTH = 100
KEEP_TAIL_LENGTH = 100


def _ensure_tmp_dir() -> Path:
    """Resolve and create the temp directory for truncated results.

    Uses ``get_wing_home()`` lazily so WING_HOME env is respected
    even if it's set after module import.
    """
    from wing.config import get_wing_home

    tmp_dir = get_wing_home() / "tmp"
    tmp_dir.mkdir(parents=True, exist_ok=True)
    return tmp_dir


def _save_full_result(result: str) -> Path:
    """将完整结果写入持久化临时文件，返回文件路径。"""
    tmp_dir = _ensure_tmp_dir()
    # 使用 NamedTemporaryFile 在指定目录下创建，delete=False 保证持久化
    with tempfile.NamedTemporaryFile(
        mode="w",
        suffix=".txt",
        prefix="wing_truncated_",
        dir=str(tmp_dir),
        delete=False,
        encoding="utf-8",
    ) as f:
        f.write(result)
        return Path(f.name)


@hooks.on("after_tool_call")
def truncate_tool_result(result: str | None, **ctx) -> str | None:
    """截断过长的 tool result。

    None 或空字符串 → 返回 None（不修改）
    20000 字符以内 → 返回 None（不修改）
    超过阈值 → 前后各保留 2000 字符，截断标记包含原始长度和临时文件路径
    """
    if not result or len(result) <= MAX_TRUNCATE_LENGTH:
        return None

    # 保存完整结果到持久化临时文件
    full_path = _save_full_result(result)

    head = result[:KEEP_HEAD_LENGTH]
    tail = result[-KEEP_TAIL_LENGTH:]
    marker = (
        f"... [truncated, original length: {len(result)} chars, "
        f"full result saved to: {full_path}. Use Read tool to view it.]"
    )
    return f"{head}\n{marker}\n{tail}"


def register_truncate_tool_result(hooks: HookRegistry) -> None:
    """注册 truncate_tool_result hook 到指定 HookRegistry。"""
    hooks.on("after_tool_call")(truncate_tool_result)
