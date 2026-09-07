# wing/agent/event_journal.py
"""EventJournal — 当前 turn 瞬态事件的内存缓冲（RAM commit）。

两次 commit 的第一层：persist=false 的瞬态事件（流式 delta、传输 ack、
状态同步）在此暂存；turn 收口时其内容合并为 Message 记录落盘（第二层，
磁盘 commit），缓冲随之清空。

合成语义（resume 重放效率的核心）：
- 直播路径照常逐包产出流式事件（前端体验不变）；
- journal 持有的状态同步做多包合成一包——
  - TextEvent / ReasoningEvent：相邻同类合并（journal 尾部为同类合成包
    则追加内容，否则新开一包）。顺序保真：交错 thinking/text 不被重排，
    只是包数减少；
  - ToolCallStreamEvent：按 tool_call_id 合并（args_fragment 拼接，
    is_final 语义保留）。不同 tool call 的参数流天然属于不同 cell，
    合并不受相邻性约束；
- 非 delta 瞬态事件（TurnStarted / ToolCallEvent 等）原样缓冲。

缓冲只存在于内存——Gateway 几乎不崩溃，不为崩溃场景的 in-flight 恢复
付出持久化代价（20/80）。

线程模型：所有调用发生在事件循环内（react_loop / 工具协程），无锁。
"""

from __future__ import annotations

from wing.event import ReasoningEvent, TextEvent, ToolCallStreamEvent, WingEvent


class EventJournal:
    """当前 turn 的瞬态事件缓冲。"""

    def __init__(self) -> None:
        self._entries: list[WingEvent] = []
        # tool_call_id → 该调用的合成流事件在 _entries 中的下标。
        # 参数流可与其他事件交错，合并不依赖相邻性。
        self._stream_index: dict[str, int] = {}

    # ── 记录 ──────────────────────────────────

    def record(self, event: WingEvent) -> None:
        """记录一个瞬态事件，流式 delta 做多包合成。"""
        if isinstance(event, TextEvent):
            last = self._entries[-1] if self._entries else None
            if isinstance(last, TextEvent):
                last.content += event.content
            else:
                self._entries.append(event)
            return

        if isinstance(event, ReasoningEvent):
            last = self._entries[-1] if self._entries else None
            if isinstance(last, ReasoningEvent):
                last.content += event.content
            else:
                self._entries.append(event)
            return

        if isinstance(event, ToolCallStreamEvent):
            idx = self._stream_index.get(event.tool_call_id)
            if idx is not None:
                merged = self._entries[idx]
                assert isinstance(merged, ToolCallStreamEvent)
                merged.args_fragment += event.args_fragment
                if event.is_final:
                    merged.is_final = True
            else:
                self._stream_index[event.tool_call_id] = len(self._entries)
                self._entries.append(event)
            return

        # 非 delta 瞬态事件：原样缓冲（保序）
        self._entries.append(event)

    # ── 快照与清理 ────────────────────────────

    def snapshot(self) -> list[WingEvent]:
        """当前缓冲的合成事件序列（浅拷贝列表，元素为合成包对象）。"""
        return list(self._entries)

    def clear(self) -> None:
        """turn 收口后清空缓冲（内容已由 Message 记录承载或按策略丢弃）。"""
        self._entries.clear()
        self._stream_index.clear()

    def __len__(self) -> int:
        return len(self._entries)
