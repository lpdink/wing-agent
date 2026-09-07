# wing/agent/inbox.py
"""Inbox — 消息队列 + feedback waiters。

封装 agent 的入站消息管理：
- asyncio.Queue 接收用户消息（Inbound）
- drain / drain_for_steer 原语
- feedback waiters：工具通过 ask_feedback 注册，用户回复按 tool_call_id 定向 resolve
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass

from wing.common.logger import log
from wing.schema import Message


@dataclass
class Inbound:
    """进入 agent 的请求，携带消息和上下文元数据。

    request_id 由 SM.post() 传入，_worker 处理时设置到 contextvar，
    使该消息触发的所有事件都携带同一个 request_id。
    """

    message: Message
    request_id: str | None = None


class Inbox:
    """Agent 入站消息管理：队列 + feedback waiters。"""

    def __init__(self) -> None:
        self._queue: asyncio.Queue[Inbound] = asyncio.Queue()
        # tool_call_id → Future，严格寻址。
        self._feedback_waiters: dict[str, asyncio.Future[str]] = {}

    # ── 队列操作 ──

    async def get(self) -> Inbound:
        """阻塞等待首条消息。"""
        return await self._queue.get()

    def drain(self) -> list[Inbound]:
        """Non-blocking drain：取出队列中所有待处理消息。"""
        items: list[Inbound] = []
        while not self._queue.empty():
            try:
                items.append(self._queue.get_nowait())
            except asyncio.QueueEmpty:
                break
        return items

    def drain_for_steer(self) -> list[Inbound]:
        """Drain 并筛选可 steer 的用户消息（user role + 非空 content）。

        返回 Inbound 列表（保留 request_id），由调用方发射
        UserMessageAcceptedEvent 并经 `format_steer_note` 格式化注入。
        """
        return [
            b for b in self.drain() if b.message.role == "user" and b.message.content
        ]

    @staticmethod
    def format_steer_note(items: list[Inbound]) -> str:
        """把 Inbound 列表格式化为 steer note。无有效消息时返回空串。"""
        notes = [b.message.content for b in items if b.message.content]
        if not notes:
            return ""
        return f"[User steer note: {'\n'.join(notes)}]\n"

    def clear(self) -> None:
        """清空队列（interrupt/shutdown 时调用）。"""
        while not self._queue.empty():
            try:
                self._queue.get_nowait()
            except asyncio.QueueEmpty:
                break

    # ── Feedback waiters ──

    @property
    def has_waiters(self) -> bool:
        """是否有工具在等待用户反馈。"""
        return bool(self._feedback_waiters)

    def pending_ask_ids(self) -> set[str]:
        """仍然挂起的 ask 的 tool_call_id 集合（权威待答集合）。

        `_feedback_waiters` 的键集合即"仍在等待回答"的 ask——resume 重放
        据此过滤链上的 AskEvent，只下发仍挂起的提问。已答（unregister_waiter）
        或已失效（cancel_all_waiters）的 ask 自然不在集合中。
        """
        return set(self._feedback_waiters.keys())

    def register_waiter(self, tool_call_id: str) -> asyncio.Future[str]:
        """注册 feedback waiter，返回 Future 供工具 await。"""
        future: asyncio.Future[str] = asyncio.get_running_loop().create_future()
        self._feedback_waiters[tool_call_id] = future
        return future

    def unregister_waiter(self, tool_call_id: str) -> None:
        """移除 waiter（超时或完成后清理）。"""
        self._feedback_waiters.pop(tool_call_id, None)

    def resolve(self, tool_call_id: str, content: str) -> bool:
        """尝试按 tool_call_id 定向 resolve feedback waiter。

        Returns:
            True 如果成功 resolve，False 如果无对应 waiter。
        """
        future = self._feedback_waiters.get(tool_call_id)
        if future is not None and not future.done():
            log.info(
                f"inbox.resolve: resolving feedback waiter "
                f"tool_call_id={tool_call_id} with '{content[:40]}'"
            )
            future.set_result(content)
            return True
        return False

    def cancel_all_waiters(self) -> None:
        """取消并清空所有 feedback waiter（interrupt/shutdown 时调用）。"""
        for future in self._feedback_waiters.values():
            future.cancel()
        self._feedback_waiters.clear()

    # ── Post（统一入口）──

    async def post(
        self,
        content: str,
        request_id: str | None = None,
        role: str = "user",
        tool_call_id: str | None = None,
    ) -> None:
        """接收用户消息或 feedback。

        Feedback 严格寻址：只有携带 tool_call_id 且命中 waiter 的消息才会
        resolve 对应 Future；无 id 或 id 已失效的消息一律进队列。
        """
        if tool_call_id is not None:
            if self.resolve(tool_call_id, content):
                return
            log.info(
                f"inbox.post: no live feedback waiter for tool_call_id="
                f"{tool_call_id}, falling through to queue"
            )

        msg = Message(role=role, content=content)  # ty: ignore
        await self._queue.put(Inbound(message=msg, request_id=request_id))
