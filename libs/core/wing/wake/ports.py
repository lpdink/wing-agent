"""wing/wake/ports.py — 唤醒服务对外的**窄接口**（组合根注入）。

``wake`` 是 L3：不能 import ``runtime`` / ``gateway``。它需要的三件事全部由这里声明、
由组合根（``WingRuntime``）实现——与远程工具「普通 Tool + 注入的 dispatch 闭包」同款思路：

- ``session_exists``：创建期校验（纯解析，不水合）；
- ``is_archived``：把「已归档」与「真不存在」的失败文案区分开（两者都是 ``state=failed``）；
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

    def is_archived(self, session_id: str) -> bool:
        """该会话是否在归档区（失败文案用：归档是可行动的结论）。"""
        ...

    async def deliver(self, *, session_id: str, content: str, request_id: str) -> None:
        """投递一条用户消息（``runtime.post`` 的窄投影）。

        Raises:
            LookupError: 会话不存在
            SessionArchivedError: 会话在归档区
        """
        ...
