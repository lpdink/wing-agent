# wing/tools/internal/feedback.py
"""等人回答的最长等待（Bash 危险命令确认与 Ask 提问共用同一份口径）。"""

#: ``ctx.ask_feedback`` 的超时（秒）。两处提问路径必须同源——改一处会让
#: 「危险命令确认」与「Ask 提问」的等待时长悄悄分叉。
FEEDBACK_TIMEOUT = 6000

__all__ = ["FEEDBACK_TIMEOUT"]
