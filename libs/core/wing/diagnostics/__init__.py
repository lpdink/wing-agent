# wing/diagnostics/__init__.py
"""wing/diagnostics 包 — 中断取证 / 诊断观测面。

11 归位：`agent/cancel_watch.py` 搬到本包（纯诊断，不参与 agent 业务）——文件名
不变，公共入口经包根 re-export：

    from wing.diagnostics import log_cancel_snapshot, watch_undead_task, InterruptLockWatch

观测面只读 task 状态、写日志，不改变任何控制流；细节与判读方式见
``diagnostics/cancel_watch.py`` 模块文档。
"""

from .cancel_watch import (
    FRAME_WALK_LIMIT,
    REPR_LIMIT,
    STACK_DEPTH,
    UNDEAD_WATCH_SCHEDULE,
    InterruptLockWatch,
    dump_task_state,
    fut_waiter_field,
    log_cancel_snapshot,
    stack_trace,
    task_label,
    task_summary,
    watch_undead_task,
)

__all__ = [
    "FRAME_WALK_LIMIT",
    "REPR_LIMIT",
    "STACK_DEPTH",
    "UNDEAD_WATCH_SCHEDULE",
    "InterruptLockWatch",
    "dump_task_state",
    "fut_waiter_field",
    "log_cancel_snapshot",
    "stack_trace",
    "task_label",
    "task_summary",
    "watch_undead_task",
]
