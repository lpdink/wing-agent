"""wing/wake/ports.py — 唤醒服务对外的**窄接口**（组合根注入）。

``wake`` 是 L3：不能 import ``runtime`` / ``gateway``。它需要的四件事全部由这里声明、
由组合根（``WingRuntime``）实现——与远程工具「普通 Tool + 注入的 dispatch 闭包」同款思路：

- ``session_exists``：创建期校验（纯解析，不水合）；
- ``ensure_durable``：持久性承诺（草稿就地物化——触发器跨重启，目标也必须跨重启）；
- ``is_archived``：把「已归档」与「真不存在」的失败文案区分开（两者都是 ``state=failed``）；
- ``session_will_run_again``：``done`` 静息判据要的运行时事实（``wing/wake/quiescence.py``）；
- ``deliver``：把一条普通用户消息投进目标会话（实现体是 ``runtime.post``，带 ``request_id``）。

窄接口的直接收益：全部生命周期 / 投递路径能用假 ports 在 pytest 里确定性覆盖。
"""

from __future__ import annotations

from typing import Protocol


class WakePorts(Protocol):
    """唤醒服务需要的会话能力（实现方是组合根，不是本包）。"""

    def session_exists(self, session_id: str) -> bool:
        """该会话是否可解析（live 区；不水合、不发事件）。"""
        ...

    def ensure_durable(self, session_id: str) -> None:
        """把会话推到「磁盘上存在」（草稿就地物化；既有会话 no-op）。

        唤醒是**持久**事实（``wake_trigger`` 跨重启），目标与 source 必须同样持久：
        第一条用户消息之前的会话只是内存草稿（见 architecture.md「会话物化」），
        登记在草稿上的触发器会在重启后投递失败。登记因此是一次显式的持久性承诺。

        Raises:
            LookupError: 会话不存在
        """
        ...

    def is_archived(self, session_id: str) -> bool:
        """该会话是否在归档区（失败文案用：归档是可行动的结论）。"""
        ...

    def session_will_run_again(self, session_id: str) -> bool:
        """该会话**自己**还会不会再跑一轮（静息判据的唯一运行时事实）。

        ``True`` = 正在跑 / 在等人类回答 / inbox 里已排上下一次的输入；``False`` = 未加载
        （逐出 / 从未水合）或已静默。不水合、不发事件、不建会话——读的是既成事实，不会把
        沉睡的会话弄醒。

        调用点是该会话自己的 ``turn_result`` 之后：那一刻 ``working`` 已经落定（worker 在
        同一个 tick 里置空、分发在下一个 tick 才跑），所以读到的 ``True`` 意味着**下一轮
        已经在跑或已排上**，而不是刚结束的这一轮。
        """
        ...

    async def deliver(self, *, session_id: str, content: str, request_id: str) -> None:
        """投递一条用户消息（``runtime.post`` 的窄投影）。

        Raises:
            LookupError: 会话不存在
            SessionArchivedError: 会话在归档区
        """
        ...
