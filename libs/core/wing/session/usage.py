# wing/session/usage.py
"""会话用量账本——内存累计 + 节流落盘。

账本只做三件事：按事件增量累计（``record_*``）、按需把**全量**写回
``metadata.usage``（``flush``）、把「多久没落盘」压在上限内（``_touch`` 的
节流）。累计是纯内存操作（每次一个加法），落盘不是一个事件一次写：回合收尾、
会话释放（``Session.aclose``）与进程退出各有显式落盘点，节流只兜住「一回合
还没结束进程就没了」的损失窗口。

记录口由 ``Session`` 从事件流驱动（``_on_usage_event``）——与直播给前端的
是同一批事件、同一批数字，因此「持久账本 == 基线 + 前端可见增量之和」，
两端不会各自漂移。
"""

from __future__ import annotations

import time
from collections.abc import Callable

from wing.store import SessionUsage

#: 回合内的落盘节流（秒）。回合收尾 / 释放 / 进程退出是显式落盘点，不依赖
#: 它；节流只保证回合中途退出时的损失不超过这个窗口。
FLUSH_INTERVAL_S = 10.0


class UsageTracker:
    """会话用量累计器（单会话、单事件循环使用，无需加锁）。"""

    def __init__(
        self,
        initial: SessionUsage | None = None,
        *,
        persist: Callable[[SessionUsage], None],
        flush_interval_s: float = FLUSH_INTERVAL_S,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        # 盘上记录是基线（resume 延续累计）；拷贝一份，未落盘前不碰 metadata。
        self._usage = (
            initial.model_copy(deep=True) if initial is not None else SessionUsage()
        )
        self._persist = persist
        self._flush_interval_s = flush_interval_s
        self._clock = clock
        self._last_flush = clock()
        self._dirty = False

    # ── 读 ───────────────────────────────────────

    def snapshot(self) -> SessionUsage:
        """当前账本的全量快照（调用方持有副本，不回写）。"""
        return self._usage.model_copy(deep=True)

    # ── 记录（事件驱动，各入口一次记一个增量）────

    def record_user_message(self) -> None:
        self._usage.user_messages += 1
        self._touch()

    def record_llm_call(
        self,
        *,
        prompt_tokens: int,
        completion_tokens: int,
        cached_tokens: int,
        duration_ms: int,
    ) -> None:
        u = self._usage
        u.llm_calls += 1
        u.prompt_tokens += max(0, prompt_tokens)
        u.completion_tokens += max(0, completion_tokens)
        u.cached_tokens += max(0, cached_tokens)
        u.llm_time_ms += max(0, duration_ms)
        self._touch()

    def record_tool_call(self, *, duration_ms: int | None) -> None:
        """记一次工具执行；``duration_ms`` 为 None 表示未真实执行（中断合成
        的结果）——不是一次调用。"""
        if duration_ms is None:
            return
        u = self._usage
        u.tool_calls += 1
        u.tool_time_ms += max(0, duration_ms)
        self._touch()

    def record_turn(self) -> None:
        self._usage.turns += 1
        self._touch()

    # ── 落盘 ─────────────────────────────────────

    def flush(self) -> None:
        """把当前全量写回（无未落盘增量时是 no-op）。

        落盘失败**绝不抛出**：保留脏标记等下一个落盘点重试（账本的一句加法
        不该把 turn 拖垮）。日志由 persist 实现负责——它知道该报什么上下文
        （如会话 id）。
        """
        if not self._dirty:
            return
        self._last_flush = self._clock()
        try:
            self._persist(self.snapshot())
        except Exception:
            self._dirty = True
            return
        self._dirty = False

    # ── 内部 ─────────────────────────────────────

    def _touch(self) -> None:
        """标记脏并执行节流落盘（距上次尝试超过间隔才真写）。"""
        self._dirty = True
        if self._clock() - self._last_flush >= self._flush_interval_s:
            self.flush()
