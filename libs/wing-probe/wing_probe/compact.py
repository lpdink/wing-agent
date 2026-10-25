"""等「后台压缩的预计算结果已落盘」的唯一原语。

后台压缩是异步 task：完成时刻不可预测，但**落盘即确定**——结果先写进 aux kv
通道（`<session_dir>/pending_compact.json`，key 见 [`PENDING_KEY`]），下一轮请求
才换入消息链。于是场景只要轮询到 aux 出现，之后的一切都是确定性的。

三个压缩相关的红线场景（自动压缩、在保留区里 fork、压缩后 rewind）都要等同一件
事：轮询实现只留这一份，失败信息才带得全（aux 路径 + 请求留档）——手写循环各自
设 magic deadline、失败只说一句「没等到」，排查时得反推是谁没发出来。
"""

from __future__ import annotations

import asyncio
import time
from typing import TYPE_CHECKING, Any

from wing_probe.history.view import HistoryView

if TYPE_CHECKING:  # 只服务类型标注：运行时避免 env / driver 的循环导入
    from wing_probe.driver import Session
    from wing_probe.probe import Probe

#: pending compact 的 aux key（`ContextManager._PENDING_COMPACT_AUX_KEY`）。
PENDING_KEY = "pending_compact"

#: 轮询预算：后台压缩是本地一次 HTTP 往返，给足余量即失败可读。
PENDING_DEADLINE = 20.0

#: 轮询间隔。
POLL_INTERVAL = 0.05


async def wait_pending(
    probe: "Probe", session: "Session", *, deadline: float = PENDING_DEADLINE
) -> dict[str, Any]:
    """轮询到 pending compact 已落盘（返回 aux 内容）。

    超时是**失败**而不是空返回：调用方要的是「待生效状态已就位」这个前提，等不到
    就该带着现场报出来。报告里带 aux 路径与请求留档——能立刻区分「压缩请求压根没
    发出」与「发出但结果没落盘」。
    """
    view: HistoryView = probe.history(session)
    expires = time.monotonic() + deadline
    while True:
        observed = view.aux(PENDING_KEY)
        if observed is not None:
            return observed
        if time.monotonic() >= expires:
            break
        await asyncio.sleep(POLL_INTERVAL)
    raise AssertionError(
        f"no pending compact under key {PENDING_KEY!r} within {deadline:.0f}s "
        f"(aux path: {view.aux_path(PENDING_KEY)}); "
        f"requests: {probe.requests.summary()}"
    )
