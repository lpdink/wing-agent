"""会话路由——钉钉 conversation ↔ Wing session 映射 + 持久化。

每个钉钉会话（conversation_id）各自维护一个"当前 session"。路由表与
进行中的 Ask 流程都落盘（容器重启不丢路由）。
"""

from __future__ import annotations

import json
import logging
import os
import tempfile
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger("wing-dingtalk.router")


@dataclass
class PendingAsk:
    """进行中的 Ask 流程（逐题问答，与 TUI AskFlow 同构）。"""

    tool_call_id: str
    session_id: str
    questions: list[dict] = field(default_factory=list)
    idx: int = 0
    answers: list[str] = field(default_factory=list)

    def current(self) -> dict | None:
        if self.idx < len(self.questions):
            return self.questions[self.idx]
        return None

    def progress(self) -> str:
        total = len(self.questions) or 1
        return f"{min(self.idx + 1, total)}/{total}"


@dataclass
class Conversation:
    """一个钉钉会话的路由状态。"""

    conversation_id: str
    # "1" = 单聊, "2" = 群聊
    kind: str = "1"
    staff_id: str = ""
    open_conversation_id: str = ""
    nick: str = ""
    session_id: str | None = None
    pending_ask: PendingAsk | None = None

    @property
    def address(self) -> str:
        """人类可读地址（日志用）。"""
        if self.kind == "2":
            return f"group:{self.open_conversation_id[:12]}"
        return f"user:{self.staff_id}"


class Router:
    """conversation_id → Conversation 路由表（文件持久化）。"""

    def __init__(self, state_path: Path) -> None:
        self._path = state_path
        self._convs: dict[str, Conversation] = {}
        self._session_index: dict[str, str] = {}  # session_id → conversation_id

    # ── 持久化 ────────────────────────────────────────────────

    def load(self) -> None:
        if not self._path.exists():
            return
        try:
            data = json.loads(self._path.read_text(encoding="utf-8"))
        except Exception as e:
            log.error(f"state file corrupt, starting fresh: {e}")
            return
        for item in data.get("conversations", []):
            ask = item.pop("pending_ask", None)
            conv = Conversation(**item)
            if ask:
                conv.pending_ask = PendingAsk(**ask)
            self._convs[conv.conversation_id] = conv
            if conv.session_id:
                self._session_index[conv.session_id] = conv.conversation_id
        log.info(f"loaded {len(self._convs)} conversation(s) from {self._path}")

    def save(self) -> None:
        payload = {
            "conversations": [self._conv_to_dict(conv) for conv in self._convs.values()]
        }
        self._path.parent.mkdir(parents=True, exist_ok=True)
        fd, tmp = tempfile.mkstemp(dir=str(self._path.parent), suffix=".tmp")
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as f:
                json.dump(payload, f, ensure_ascii=False, indent=2)
            os.replace(tmp, self._path)
        except Exception:
            try:
                os.unlink(tmp)
            except OSError:
                pass
            raise

    @staticmethod
    def _conv_to_dict(conv: Conversation) -> dict[str, Any]:
        d = asdict(conv)
        if d["pending_ask"] is None:
            d.pop("pending_ask")
        return d

    # ── 路由 ──────────────────────────────────────────────────

    def upsert_from_message(self, data: dict) -> Conversation:
        """从入站钉钉消息更新（或创建）会话记录。"""
        conv_id = str(data.get("conversationId", ""))
        conv = self._convs.get(conv_id)
        if conv is None:
            conv = Conversation(conversation_id=conv_id)
            self._convs[conv_id] = conv
        conv.kind = str(data.get("conversationType", "1"))
        conv.staff_id = str(data.get("senderStaffId", "") or "")
        conv.open_conversation_id = str(data.get("conversationId", ""))
        conv.nick = str(data.get("senderNick", "") or "")
        return conv

    def get(self, conversation_id: str) -> Conversation | None:
        return self._convs.get(conversation_id)

    def conversation_for_session(self, session_id: str) -> Conversation | None:
        conv_id = self._session_index.get(session_id)
        return self._convs.get(conv_id) if conv_id else None

    def all_conversations(self) -> list[Conversation]:
        return list(self._convs.values())

    def bound_conversations(self) -> list[Conversation]:
        """已绑定 wing session 的会话。"""
        return [c for c in self._convs.values() if c.session_id]

    def set_session(self, conv: Conversation, session_id: str) -> None:
        """切换会话的当前 session（更新反向索引并落盘）。"""
        if conv.session_id and conv.session_id in self._session_index:
            self._session_index.pop(conv.session_id, None)
        conv.session_id = session_id
        conv.pending_ask = None
        self._session_index[session_id] = conv.conversation_id
        self.save()

    def all_session_ids(self) -> list[str]:
        return [c.session_id for c in self._convs.values() if c.session_id]
