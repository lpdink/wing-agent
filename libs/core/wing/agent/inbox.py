# wing/agent/inbox.py
"""Inbox — 消息队列 + feedback waiters。

封装 agent 的入站消息管理：
- 用户消息队列（Inbound）
- 批量 drain 与只读队列快照
- hold / release：取出口闸门（手动压缩这类会话级独占操作期间不交付输入）
- feedback waiters：工具通过 ask_feedback 注册，用户回复按 tool_call_id 定向 resolve
"""

from __future__ import annotations

import asyncio
from collections import deque
from dataclasses import dataclass

from wing.common.logger import log, log_excerpt
from wing.event import PendingUserMessage
from wing.schema import MediaRef, Message


@dataclass
class Inbound:
    """进入 agent 的请求，携带消息和上下文元数据。

    request_id 由 SM.post() 传入，_worker 处理时设置到 contextvar，
    使该消息触发的所有事件都携带同一个 request_id。
    """

    message: Message
    request_id: str | None = None

    def pending_user_message(self) -> PendingUserMessage | None:
        if (
            self.request_id is None
            or self.message.role != "user"
            or not (self.message.content or self.message.media)
        ):
            return None
        return PendingUserMessage(
            origin_request_id=self.request_id,
            content=self.message.content or "",
            media=self.message.media or [],
        )


class Inbox:
    """Agent 入站消息管理：队列 + feedback waiters + 取出口闸门。"""

    def __init__(self) -> None:
        self._queue: deque[Inbound] = deque()
        # tool_call_id → Future，严格寻址。
        self._feedback_waiters: dict[str, asyncio.Future[str]] = {}
        # 取出口闸门：hold 期间 `get()` 不把输入交给 worker（手动压缩是会话级独占
        # 操作，压缩期间到达的消息留在队列里，压缩结束后才开始处理）。
        self._held = False
        self._ready = asyncio.Event()

    # ── 队列操作 ──

    def hold(self) -> None:
        """暂停取出口（与 `release` 成对，由独占操作的持有者调用）。

        已排队的输入**留在队列里**：`has_pending` 照常为真（逐出 / 归档据此钉住
        会话），worker 停在下一次 `get()` 的等锁上，而不是把消息取走干等。
        """
        self._held = True
        self._ready.clear()

    def release(self) -> None:
        """恢复取出口（幂等）。"""
        self._held = False
        if self._queue:
            self._ready.set()

    @property
    def held(self) -> bool:
        """取出口是否被独占操作持有。"""
        return self._held

    async def get(self) -> Inbound:
        """阻塞等待首条消息（取出口被 hold 时等待，不取走任何条目）。"""
        while self._held or not self._queue:
            await self._ready.wait()
        first = self._queue.popleft()
        if not self._queue:
            self._ready.clear()
        return first

    def _enqueue(self, item: Inbound) -> None:
        self._queue.append(item)
        if not self._held:
            self._ready.set()

    def drain(self) -> list[Inbound]:
        """Non-blocking drain：取出队列中所有待处理消息。"""
        items = self.snapshot()
        self._queue.clear()
        self._ready.clear()
        return items

    def snapshot(self) -> list[Inbound]:
        return list(self._queue)

    def clear(self) -> list[Inbound]:
        """清空队列（interrupt 时调用），返回被丢弃的条目。

        返回值让调用方（`WingAgent.interrupt`）能把「哪些输入被放弃」显式
        告知前端（`InterruptedEvent.dropped_request_ids`）——前端只把这些
        消息标为 discarded，不误伤锁等待期间新到的消息。
        """
        return self.drain()

    @property
    def has_pending(self) -> bool:
        """队列里是否有尚未被 worker 取走的输入。

        供会话逐出判定用：消息已入队但 turn 还没开始（worker 尚未
        `_set_working(True)`），此时 status 仍是 idle——不看队列会误判为
        「空闲可逐出」并把这条输入连同它将要驱动的那一轮一起掐掉。
        """
        return bool(self._queue)

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
                "inbox.resolve: resolving feedback waiter tool_call_id=%s with '%s'",
                tool_call_id,
                log_excerpt(content, 40),
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
        media: list[MediaRef] | None = None,
    ) -> Inbound | None:
        """接收用户消息或 feedback。返回实际入队的输入。

        Feedback 严格寻址：只有携带 tool_call_id 且命中 waiter 的消息才会
        resolve 对应 Future；无 id 或 id 已失效的消息一律进队列。

        media 进 ``Message(media=...)``（空列表由 Message 归一为 None）。**定向
        答复只承载文本**：命中 waiter 时 media 无处安放（Ask 答复通道是纯文本），
        记 WARN 后按文本 resolve——不发明新语义，也不静默吞掉调用方的意图。
        """
        if tool_call_id is not None:
            if self.resolve(tool_call_id, content):
                if media:
                    log.warning(
                        f"inbox.post: feedback answer for tool_call_id="
                        f"{tool_call_id} carries {len(media)} media ref(s); "
                        "directed answers are text-only, media ignored"
                    )
                return None
            log.info(
                f"inbox.post: no live feedback waiter for tool_call_id="
                f"{tool_call_id}, falling through to queue"
            )

        msg = Message(role=role, content=content, media=media)  # ty: ignore
        item = Inbound(message=msg, request_id=request_id)
        self._enqueue(item)
        return item
