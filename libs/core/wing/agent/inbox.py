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
from wing.schema import MediaRef, Message


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
        """部分 drain：只取**队首连续**的「无 media 纯文本」user 消息（D7）。

        返回 Inbound 列表（保留 request_id），由调用方发射
        UserMessageAcceptedEvent 并经 `format_steer_note` 格式化注入。

        队列语义 = 「steer 变 next turn」：遇到第一条不可 steer 的消息（带 media
        的贴图消息 / 空 content / 非 user）**立即停止**——它与其后所有消息（含
        后续纯文本）按原序留在队列，等下一轮 drain-and-merge 一起进上下文。
        首条带图消息因此天然阻断其后消息：图片与文本同轮进模型，顺序不被打乱
        （D5：文本/图片分开承载、不承诺交错顺序——但绝不把队列切碎）。

        实现是「先全部 drain、前缀留给调用方、其余按原序回填」：方法体内**无
        await**，单线程事件循环下取空与回填之间不会与新的 `post` 交错；无界
        队列的 `put_nowait` 恒不阻塞。旧实现的过滤式 drain 会把不合条件的条目
        **直接丢弃**（带图消息静默消失）——这里修掉的就是那条数据丢失路径。
        """
        batch = self.drain()
        steerable: list[Inbound] = []
        index = 0
        while index < len(batch):
            candidate = batch[index]
            if not (
                candidate.message.role == "user"
                and candidate.message.content
                and not candidate.message.media
            ):
                break
            steerable.append(candidate)
            index += 1
        for item in batch[index:]:
            self._queue.put_nowait(item)
        return steerable

    @staticmethod
    def format_steer_note(items: list[Inbound]) -> str:
        """把 Inbound 列表格式化为 steer note。无有效消息时返回空串。"""
        notes = [b.message.content for b in items if b.message.content]
        if not notes:
            return ""
        return f"[User steer note: {'\n'.join(notes)}]\n"

    def clear(self) -> list[Inbound]:
        """清空队列（interrupt 时调用），返回被丢弃的条目。

        返回值让调用方（`WingAgent.interrupt`）能把「哪些输入被放弃」显式
        告知前端（`InterruptedEvent.dropped_request_ids`）——前端只把这些
        消息标为 discarded，不误伤锁等待期间新到的消息。
        """
        dropped: list[Inbound] = []
        while not self._queue.empty():
            try:
                dropped.append(self._queue.get_nowait())
            except asyncio.QueueEmpty:
                break
        return dropped

    @property
    def has_pending(self) -> bool:
        """队列里是否有尚未被 worker 取走的输入。

        供会话逐出判定用：消息已入队但 turn 还没开始（worker 尚未
        `_set_working(True)`），此时 status 仍是 idle——不看队列会误判为
        「空闲可逐出」并把这条输入连同它将要驱动的那一轮一起掐掉。
        """
        return not self._queue.empty()

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
        media: list[MediaRef] | None = None,
    ) -> None:
        """接收用户消息或 feedback。

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
                return
            log.info(
                f"inbox.post: no live feedback waiter for tool_call_id="
                f"{tool_call_id}, falling through to queue"
            )

        msg = Message(role=role, content=content, media=media)  # ty: ignore
        await self._queue.put(Inbound(message=msg, request_id=request_id))
