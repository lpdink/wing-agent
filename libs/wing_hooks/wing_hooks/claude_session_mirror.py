"""claude_session_mirror hook — 把 wing 会话投影成 Claude 转录格式（只读）。

CloudCLI（siteboon/claudecodeui）的**会话列表**来自递归扫描
``~/.claude/projects/**/*.jsonl``，**历史**来自逐行重放同一个 JSONL（Claude
转录格式）。本 hook 订阅全局 EventBus，把 wing 会话写成同构的投影：

    <claude-home>/projects/<cwd 编码>/<wing-session-id>.jsonl

映射（每行一条 JSON 记录，append-only；``parentUuid`` 以上一条投影行线性串联）：

- ``user_message_accepted``  → ``type="user"``，``message.content`` 一个 text 块；
- ``assistant_turn``        → ``type="assistant"``，``message.content`` 为
  thinking / text / tool_use 块（事件里的原序；``message.model`` 随行）；
- ``tool_result_turn``      → ``type="user"``，``message.content`` 一条
  ``tool_result`` 块（``tool_use_id`` / ``content`` / ``is_error``）；
- ``session_state_changed.title`` → ``type="custom-title"``（会话改名，标题来源）；
- 每条 user 行之后补一行 ``type="last-prompt"``（CloudCLI 标题的第二来源，
  ``leafUuid`` 指向该 user 行）。

每行携带 ``uuid`` / ``parentUuid`` / ``sessionId``（= wing 原生 session id）/
``cwd`` / ``timestamp``。会话目录 = ``cwd`` 里非字母数字字符一律替换成 ``-``
（Claude Code 的 ``projects/<encoded-cwd>`` 约定）。

定位是**只读投影**（interop export），不是 wing 的事实来源，因此：

- 回调路径零阻塞：只做类型分派 + ``Queue.put_nowait``，全部 IO（目录创建 /
  序列化 / 写盘）在一个 daemon 写线程里串行完成；队列满即丢弃并 warning
  （宁可少写，不可积压）；
- 任何失败（目录不可用 / 序列化失败 / 未预期异常）就地捕获、log 降级，
  绝不向 EventBus 或 agent 主流程抛；
- reload 幂等：投影器实例挂在 ``event_bus`` 单例的私有属性上，重复
  ``load_hooks()``（网关启动 / ``POST /api/system/reload``）只重新订阅一次，
  挂起行 / 已定 cwd / 写线程跨 reload 存活；
- 跨进程续链：网关重启（或重装投影器）后，会话首次落盘前若目标文件已存在，
  从尾部读回最后一个完整行的游标（``uuid``，或 ``last-prompt.leafUuid``）作为
  初始 ``last_uuid``——重启后的段落续到旧链上，全文件保持单根线性链；文件若以
  半截行收尾，先补一个换行给残行封口；
- 落盘纪律：游标只在**写成功之后**提交——单批写盘失败时游标不动（下一批重链到
  最后一个已落盘行），文件里绝不出现悬空 ``parentUuid``。

启用（任选其一）：

- ``cp libs/wing_hooks/wing_hooks/claude_session_mirror.py ~/.wing/hooks/``，
  配置 ``hooks: ~/.wing/hooks/*.py``；``POST /api/system/reload`` 即生效；
- 或 ``from wing_hooks.claude_session_mirror import install`` 后显式
  ``install(bus=..., claude_home=...)``。

**注意 import 侧效应**：本模块装载（import / 被 ``load_hooks`` exec）即
``install()`` 到**全局** ``event_bus`` 上，默认落点是 ``~/.claude``；此后显式
``install(bus, claude_home=X)`` 会复用已装实例、``X`` 不生效。要换根请先设
``WING_CLAUDE_MIRROR_HOME`` / ``CLAUDE_CONFIG_DIR``，或先 ``uninstall()`` 再显式
安装（测试即这么做）。

cwd 来源优先级（决定文件落点，见任务 design.md D3）：``before_session_start``
（创建 / fork）→ ``session_init`` 事件（订阅时下发）→ ``sync_session`` 事件的
``agent.workspace`` → file 后端 ``metadata.json`` 的 ``workspace`` 兜底（resume
路径没有前两类来源）；全部未知时该会话的行**挂起**（有上限），宁可不写也不写错目录。
"""

from __future__ import annotations

import json
import os
import queue
import re
import threading
import uuid as _uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from wing.common.logger import log
from wing.event import (
    AssistantTurnEvent,
    SessionInitEvent,
    SessionStateChangedEvent,
    SyncSessionEvent,
    ToolResultTurnEvent,
    UserMessageAcceptedEvent,
)
from wing.event_bus import EventBus, event_bus
from wing.hooks import HookRegistry, hooks

#: 总开关：置 False 后 reload 网关即卸载（不再订阅；已订阅的撤销）。
ENABLED = True

#: 覆盖 ``~/.claude`` 根的环境变量（其次 ``CLAUDE_CONFIG_DIR``，最后 home）。
HOME_ENV = "WING_CLAUDE_MIRROR_HOME"
CLAUDE_CONFIG_DIR_ENV = "CLAUDE_CONFIG_DIR"

#: 队列上限：文件系统抖动时宁可丢弃也不无界积压（与 ~/.wing/hooks 的
#: dingtalk_turn_notify 同一策略）。
QUEUE_MAX = 4096

#: 每会话"等待 cwd"的挂起行上限：超出丢最旧（cwd 未知时绝不猜目录）。
MAX_PENDING_ROWS = 512

#: 单块内容安全上限（1 MiB）：超出截断并追加可见标记（病态载荷兜底）。
MAX_CONTENT_CHARS = 1024 * 1024

#: ``lastPrompt`` 上限（消费端只取前 120 字符做标题）。
LAST_PROMPT_MAX_CHARS = 2000

#: 链游标种子（跨进程续链）从文件尾部读的窗口：先小后大，覆盖"最后一行特别长"
#: （单行上限见 MAX_CONTENT_CHARS）的极端情况；读完仍找不到完整行就放弃。
SEED_WINDOW_START = 64 * 1024
SEED_WINDOW_MAX = 8 * 1024 * 1024

#: 挂在 event_bus 单例上的标记：投影器实例 / 安装锁（跨 reload 复用与互斥）。
_MARKER = "_claude_session_mirror_state"
_LOCK_MARKER = "_claude_session_mirror_lock"

#: 当前活动的投影器（模块级；每次 load_hooks 重新 exec 后由 _install_default
#: 指回单例上的实例）——``before_session_start`` 通过它登记 session → workspace。
_ACTIVE: "ClaudeSessionMirror | None" = None


# ============================================================
# 纯函数：行构造与格式化
# ============================================================


def encode_cwd(cwd: str) -> str:
    """Claude Code 的 project 目录编码：非字母数字一律替换成 ``-``。"""
    return re.sub(r"[^A-Za-z0-9]", "-", cwd)


def _cap(text: str, limit: int = MAX_CONTENT_CHARS) -> str:
    """超限截断 + 可见标记（不改动未超限的内容）。"""
    if len(text) <= limit:
        return text
    return text[:limit] + (
        f"\n... [truncated by wing claude mirror: original {len(text)} chars]"
    )


def _iso_ts(value: Any) -> str:
    """事件时刻 → Claude 风格的 UTC ISO 时间戳；非法值回落 now。"""
    dt = value if isinstance(value, datetime) else datetime.now(timezone.utc)
    if dt.tzinfo is None:
        dt = dt.astimezone()  # naive = 本地时刻
    return (
        dt.astimezone(timezone.utc)
        .isoformat(timespec="milliseconds")
        .replace("+00:00", "Z")
    )


def _event_uuid(event: Any) -> str:
    raw = getattr(event, "uuid", None)
    return raw if isinstance(raw, str) and raw else str(_uuid.uuid4())


def _base_row(event: Any, row_type: str) -> dict[str, Any]:
    """会话行骨架：``cwd`` / ``parentUuid`` 由写线程在落盘时刻填充。"""
    return {
        "type": row_type,
        "uuid": _event_uuid(event),
        "parentUuid": None,
        "sessionId": event.session_id,
        "cwd": None,
        "timestamp": _iso_ts(getattr(event, "created_at", None)),
    }


def _user_row(event: UserMessageAcceptedEvent) -> dict[str, Any]:
    row = _base_row(event, "user")
    row["isMeta"] = False
    row["message"] = {
        "role": "user",
        "content": [{"type": "text", "text": _cap(event.content or "")}],
    }
    return row


def _assistant_blocks(raw_blocks: list[dict]) -> list[dict[str, Any]]:
    """事件块 → 转录块（thinking / text / tool_use；空文本块丢弃）。"""
    blocks: list[dict[str, Any]] = []
    for raw in raw_blocks or []:
        if not isinstance(raw, dict):
            continue
        kind = raw.get("type")
        if kind == "thinking" and isinstance(raw.get("thinking"), str):
            thinking = _cap(raw["thinking"])
            if thinking:
                blocks.append({"type": "thinking", "thinking": thinking})
        elif kind == "text" and isinstance(raw.get("text"), str):
            text = _cap(raw["text"])
            if text:
                blocks.append({"type": "text", "text": text})
        elif kind == "tool_use":
            blocks.append(
                {
                    "type": "tool_use",
                    "id": str(raw.get("id") or ""),
                    "name": str(raw.get("name") or ""),
                    "input": raw.get("input")
                    if isinstance(raw.get("input"), dict)
                    else {},
                }
            )
    return blocks


def _assistant_row(event: AssistantTurnEvent) -> dict[str, Any] | None:
    """助手行；无可用内容块时返回 None（不写空行）。"""
    blocks = _assistant_blocks(event.content_blocks)
    if not blocks:
        return None
    row = _base_row(event, "assistant")
    message: dict[str, Any] = {"role": "assistant", "content": blocks}
    if event.model:
        message["model"] = event.model
    row["message"] = message
    return row


def _tool_result_row(event: ToolResultTurnEvent) -> dict[str, Any]:
    row = _base_row(event, "user")
    row["message"] = {
        "role": "user",
        "content": [
            {
                "type": "tool_result",
                "tool_use_id": event.tool_use_id,
                "content": _cap(event.content or ""),
                "is_error": bool(event.is_error),
            }
        ],
    }
    return row


def _last_prompt_row(event: UserMessageAcceptedEvent, leaf_uuid: str) -> dict[str, Any]:
    """``last-prompt`` 行：CloudCLI 的标题回落来源（保留最后一次提示）。"""
    return {
        "type": "last-prompt",
        "lastPrompt": _cap(event.content or "", LAST_PROMPT_MAX_CHARS),
        "leafUuid": leaf_uuid,
        "sessionId": event.session_id,
    }


def _title_row(session_id: str, title: str) -> dict[str, Any]:
    """``custom-title`` 行：wing 会话名 → CloudCLI 的标题首选来源。"""
    return {"type": "custom-title", "customTitle": title, "sessionId": session_id}


# ============================================================
# cwd 解析
# ============================================================


def default_claude_home() -> Path:
    """``~/.claude``（``WING_CLAUDE_MIRROR_HOME`` / ``CLAUDE_CONFIG_DIR`` 可覆盖）。"""
    override = os.environ.get(HOME_ENV) or os.environ.get(CLAUDE_CONFIG_DIR_ENV)
    if override:
        return Path(override).expanduser()
    return Path.home() / ".claude"


def normalize_workspace(raw: Any) -> str | None:
    """workspace 值 → 绝对路径字符串（空 / 非法值 = None）。"""
    if not isinstance(raw, str) or not raw.strip():
        return None
    try:
        return str(Path(raw).expanduser().resolve())
    except OSError:
        return raw


def _workspace_from_metadata(session_id: str) -> str | None:
    """兜底：file 后端 ``metadata.json`` 的 workspace（resume 路径没有事件来源）。

    只读探测（写入方永远是 SessionStore）；非法 id / 非 file 后端 / 任何异常
    都只是"没有兜底"，不抛。
    """
    try:
        from wing.common.utils import is_valid_session_id
        from wing.config.loader import get_wing_home

        if not is_valid_session_id(session_id):
            return None
        env_root = os.environ.get("WING_SESSIONS_PATH")
        root = Path(env_root).expanduser() if env_root else get_wing_home() / "sessions"
        data = json.loads(
            (root / session_id / "metadata.json").read_text(encoding="utf-8")
        )
        return normalize_workspace(data.get("workspace"))
    except Exception as e:  # noqa: BLE001 — 兜底路径绝不外抛
        log.debug(f"[claude-mirror] metadata probe failed for {session_id}: {e}")
        return None


# ============================================================
# 链游标种子（跨进程续链）
# ============================================================


def _cursor_of(row: dict[str, Any]) -> str | None:
    """一条投影行贡献的链游标：自身 ``uuid``，或 ``last-prompt`` 的 ``leafUuid``。"""
    uuid = row.get("uuid")
    if isinstance(uuid, str) and uuid:
        return uuid
    if row.get("type") == "last-prompt":
        leaf = row.get("leafUuid")
        if isinstance(leaf, str) and leaf:
            return leaf
    return None


def _ends_mid_line(path: Path) -> bool:
    """文件已存在且以半截行收尾（最后一个字节不是换行）——崩溃残行的判据。

    这时直接 append 会把新行拼到残行上（新行随之不可解析），所以首次落盘时
    先补一个换行把残行封口（见 ``_write_rows`` 的 ``needs_separator``）。
    """
    try:
        with path.open("rb") as handle:
            handle.seek(0, os.SEEK_END)
            if handle.tell() == 0:
                return False
            handle.seek(-1, os.SEEK_END)
            return handle.read(1) != b"\n"
    except OSError:
        return False


def _seed_last_uuid(path: Path) -> str | None:
    """从已存在的投影文件尾部恢复链游标（网关重启后接着写，见 design R1/S1）。

    从尾部窗口往前找第一条能解析出游标的**完整**行：半截行（崩溃留下的、
    或读到写了一半的行）与非法 JSON 行一律跳过。窗口不够（最后一行特别长）
    时按倍率放大重读；仍找不到就返回 None（从新根开始——绝不猜）。
    """
    try:
        size = path.stat().st_size
    except OSError:
        return None
    if size <= 0:
        return None
    window = SEED_WINDOW_START
    while True:
        start = max(0, size - window)
        try:
            with path.open("rb") as handle:
                handle.seek(start)
                raw = handle.read()
        except OSError:
            return None
        text = raw.decode("utf-8", errors="ignore")
        if start > 0:
            # 窗口首部必然是半截行：从第一个换行之后开始（窗口整体落在一行内则无完整行）
            head, sep, tail = text.partition("\n")
            text = tail if sep else ""
        for line in reversed(text.splitlines()):
            line = line.strip()
            if not line:
                continue
            try:
                row = json.loads(line)
            except ValueError:
                continue
            if not isinstance(row, dict):
                continue
            cursor = _cursor_of(row)
            if cursor:
                return cursor
        if start == 0 or window >= SEED_WINDOW_MAX:
            return None
        window = min(window * 8, SEED_WINDOW_MAX)


# ============================================================
# 会话运行态
# ============================================================


class _SessionCtx:
    """一个会话的投影运行态（只有写线程访问）。

    刻意**不用 @dataclass**：hook 文件是被 ``load_hooks`` 以
    ``spec_from_file_location + exec_module`` 装载的（不注册进 ``sys.modules``），
    而 dataclasses 在 Python 3.12 的 KW_ONLY 探测会走
    ``sys.modules.get(cls.__module__).__dict__`` → None → AttributeError，
    整个 hook 文件装载失败（临时 E2E 实测）。
    """

    __slots__ = (
        "session_id",
        "cwd",
        "path",
        "last_uuid",
        "title",
        "metadata_probed",
        "needs_separator",
        "pending",
    )

    def __init__(self, session_id: str) -> None:
        self.session_id = session_id
        self.cwd: str | None = None
        self.path: Path | None = None
        self.last_uuid: str | None = None
        self.title: str | None = None
        self.metadata_probed = False
        self.needs_separator = False
        """已存在文件以半截行收尾：下一批落盘前先补一个换行（见 _ends_mid_line）。"""
        self.pending: list[dict[str, Any]] = []


# ============================================================
# 投影器
# ============================================================


class ClaudeSessionMirror:
    """EventBus → Claude 转录 JSONL 的只读投影器。

    订阅回调（``_on_event``）只做类型分派 + 入队；写线程（``_write_rows``
    路径）串行完成 cwd 解析 / 目录创建 / 序列化 / 落盘。
    """

    def __init__(self, bus: EventBus, claude_home: Path | str | None = None) -> None:
        self._bus = bus
        self._home = (
            Path(claude_home) if claude_home is not None else default_claude_home()
        )
        self._queue: queue.Queue[tuple[Any, ...]] = queue.Queue(maxsize=QUEUE_MAX)
        self._lock = threading.Lock()
        self._thread: threading.Thread | None = None
        self._attached = False
        self._dropped = 0
        self._sessions: dict[str, _SessionCtx] = {}

    # ── 生命周期 ──────────────────────────────

    def attach(self) -> None:
        """订阅（幂等）。"""
        if self._attached:
            return
        self._attached = True
        self._bus.subscribe(self._on_event)

    def detach(self) -> None:
        """撤销订阅（幂等；已在队列里的记录仍会被写完）。"""
        if not self._attached:
            return
        self._attached = False
        try:
            self._bus.unsubscribe(self._on_event)
        except Exception as e:  # noqa: BLE001 — 卸载路径不抛
            log.warning(f"[claude-mirror] detach failed: {e}")

    def note_session(self, session_id: str | None, workspace: str | None) -> None:
        """登记 session → workspace（``before_session_start`` 调用，零阻塞）。"""
        if not session_id or not workspace:
            return
        self._enqueue(("note", session_id, workspace))

    def flush(self, timeout: float = 5.0) -> bool:
        """等待已入队记录全部落盘（测试 / 停机）；超时或队列满返回 False。"""
        done = threading.Event()
        try:
            if not self._enqueue(("flush", done)):
                return False
        except Exception:  # noqa: BLE001 — flush 自身绝不抛
            return False
        return done.wait(timeout)

    # ── 订阅回调（EventBus 线程） ──────────────

    def _on_event(self, event: Any) -> None:
        """EventBus 回调：只做类型分派 + 入队；异常就地捕获。"""
        try:
            if isinstance(
                event,
                (
                    UserMessageAcceptedEvent,
                    AssistantTurnEvent,
                    ToolResultTurnEvent,
                    SessionInitEvent,
                    SyncSessionEvent,
                    SessionStateChangedEvent,
                ),
            ):
                self._enqueue(("event", event))
        except Exception as e:  # noqa: BLE001 — 绝不向 EventBus 抛
            log.warning(f"[claude-mirror] handler error: {e}")

    def _enqueue(self, item: tuple[Any, ...]) -> bool:
        """懒启动写线程 + 非阻塞入队；队列满即丢弃并告警（返回是否入队成功）。"""
        self._ensure_worker()
        try:
            self._queue.put_nowait(item)
        except queue.Full:
            self._dropped += 1
            log.warning(
                f"[claude-mirror] queue full — dropped record "
                f"(total dropped: {self._dropped})"
            )
            return False
        return True

    def _ensure_worker(self) -> None:
        if self._thread is not None:
            return
        with self._lock:
            if self._thread is not None:
                return
            thread = threading.Thread(
                target=self._run, name="claude-session-mirror", daemon=True
            )
            self._thread = thread
            thread.start()

    # ── 写线程 ────────────────────────────────

    def _run(self) -> None:
        while True:
            item = self._queue.get()
            try:
                if item[0] == "flush":
                    item[1].set()
                else:
                    self._process(item)
            except Exception as e:  # noqa: BLE001 — 单条失败不杀线程
                log.warning(f"[claude-mirror] write error: {e}")
            finally:
                self._queue.task_done()

    def _process(self, item: tuple[Any, ...]) -> None:
        if item[0] == "note":
            self._note_session(item[1], item[2])
            return

        event = item[1]
        session_id = getattr(event, "session_id", None)
        if not session_id:
            return  # 无归属事件无法归档

        if isinstance(event, UserMessageAcceptedEvent):
            row = _user_row(event)
            self._append(session_id, row)
            self._append(session_id, _last_prompt_row(event, row["uuid"]))
        elif isinstance(event, AssistantTurnEvent):
            row = _assistant_row(event)
            if row is not None:
                self._append(session_id, row)
        elif isinstance(event, ToolResultTurnEvent):
            self._append(session_id, _tool_result_row(event))
        elif isinstance(event, SessionInitEvent):
            self._note_session(session_id, normalize_workspace(event.cwd))
        elif isinstance(event, SyncSessionEvent):
            agent = event.agent
            self._note_session(
                session_id, normalize_workspace(agent.workspace if agent else None)
            )
        elif isinstance(event, SessionStateChangedEvent):
            self._note_title(session_id, event.title)

    def _note_session(self, session_id: str, workspace: str | None) -> None:
        """登记 cwd（一旦确定即冻结）并补写挂起行。"""
        if not session_id or not workspace:
            return
        ctx = self._sessions.get(session_id)
        if ctx is None:
            ctx = _SessionCtx(session_id=session_id)
            self._sessions[session_id] = ctx
        if ctx.cwd is not None:
            return  # 已冻结：不迁移历史文件（见 design D3）
        ctx.cwd = workspace
        if ctx.pending:
            rows, ctx.pending = ctx.pending, []
            self._write_rows(ctx, rows)

    def _note_title(self, session_id: str, title: str | None) -> None:
        if not title:
            return
        ctx = self._sessions.get(session_id)
        if ctx is None:
            ctx = _SessionCtx(session_id=session_id)
            self._sessions[session_id] = ctx
        if ctx.title == title:
            return
        ctx.title = title
        self._append(session_id, _title_row(session_id, title))

    def _append(self, session_id: str, row: dict[str, Any]) -> None:
        """写一行（cwd 未知时挂起；写失败由调用方兜底）。"""
        ctx = self._sessions.get(session_id)
        if ctx is None:
            ctx = _SessionCtx(session_id=session_id)
            self._sessions[session_id] = ctx
        if ctx.cwd is None and not ctx.metadata_probed:
            ctx.metadata_probed = True
            ctx.cwd = _workspace_from_metadata(session_id)
            if ctx.pending:
                rows, ctx.pending = ctx.pending, []
                self._write_rows(ctx, rows)
        if ctx.cwd is None:
            self._park(ctx, row)
            return
        self._write_rows(ctx, [row])

    def _park(self, ctx: _SessionCtx, row: dict[str, Any]) -> None:
        ctx.pending.append(row)
        if len(ctx.pending) > MAX_PENDING_ROWS:
            ctx.pending.pop(0)
            log.warning(
                f"[claude-mirror] dropped parked row (cwd still unknown) for "
                f"session {ctx.session_id}; {MAX_PENDING_ROWS} pending limit"
            )

    def _write_rows(self, ctx: _SessionCtx, rows: list[dict[str, Any]]) -> None:
        """串行落盘：补链拓扑（parentUuid）+ cwd，一次 append 写完这批行。

        游标只在**写成功之后**提交（S2）：本批写盘失败（ENOSPC / 权限 / 目录
        不可写…）时 ``ctx.last_uuid`` 不动，下一批自动重链到最后一个**已落盘**
        行，不会在文件里留下悬空 ``parentUuid``。
        """
        if not rows:
            return
        path = self._resolve_path(ctx)  # 首次落盘：定路径 + 续链种子（必须先于取游标）
        cursor = ctx.last_uuid
        for row in rows:
            if "uuid" in row:
                row["parentUuid"] = cursor
                cursor = row["uuid"]
            if "cwd" in row:
                row["cwd"] = ctx.cwd
        payload = ("\n" if ctx.needs_separator else "") + "".join(
            json.dumps(row, ensure_ascii=False, default=str) + "\n" for row in rows
        )
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a", encoding="utf-8") as handle:
            handle.write(payload)
        ctx.last_uuid = cursor
        ctx.needs_separator = False

    def _resolve_path(self, ctx: _SessionCtx) -> Path:
        """首次落盘时定下文件路径；目标文件已存在则续链（S1）+ 残行封口。

        只做一次（``ctx.path`` 一旦确定即复用）：同一个投影器实例不会为同一
        会话反复读尾部，重启后的新实例则在这里把旧链接上。
        """
        if ctx.path is None:
            ctx.path = (
                self._home
                / "projects"
                / encode_cwd(ctx.cwd or "")
                / f"{ctx.session_id}.jsonl"
            )
            ctx.needs_separator = _ends_mid_line(ctx.path)
            ctx.last_uuid = _seed_last_uuid(ctx.path)
        return ctx.path


# ============================================================
# 钩子入口（before_session_start 用）（模块装载时注册）
# ============================================================


def claude_mirror_session_start(session: Any, **ctx: Any) -> None:
    """``before_session_start``：登记 session → workspace（投影文件的落点）。

    返回 None = 不改写 hook 管道的值（不干涉会话初始化）。
    """
    mirror = _ACTIVE
    if mirror is None:
        return
    try:
        workspace = normalize_workspace(getattr(session, "session_workspace", None))
        mirror.note_session(getattr(session, "session_id", None), workspace)
    except Exception as e:  # noqa: BLE001 — 绝不向会话初始化路径抛
        log.warning(f"[claude-mirror] session start hook error: {e}")


def register_claude_session_mirror(registry: HookRegistry) -> None:
    """把 ``before_session_start`` 登记 handler 注册到指定 HookRegistry。"""
    registry.on("before_session_start")(claude_mirror_session_start)


@hooks.on("before_session_start")
def _on_before_session_start(session: Any, **ctx: Any) -> None:
    """全局 registry 的登记入口（load_hooks 每次 reload 后由本模块重新注册）。"""
    claude_mirror_session_start(session, **ctx)


# ============================================================
# 安装 / 卸载（event_bus 单例上的幂等标记）
# ============================================================


def install(
    bus: EventBus = event_bus, claude_home: Path | str | None = None
) -> ClaudeSessionMirror | None:
    """在 ``bus`` 上幂等安装投影器（reload 安全：同一 bus 只保留一个订阅）。

    已安装时复用单例上的实例（挂起行 / 已定 cwd / 写线程跨 reload 存活），
    此时新的 ``claude_home`` 不生效（落点以首次安装为准）。
    """
    global _ACTIVE
    if not ENABLED:
        uninstall(bus)
        return None
    lock = getattr(bus, _LOCK_MARKER, None)
    if lock is None:
        lock = threading.Lock()
        setattr(bus, _LOCK_MARKER, lock)
    with lock:
        mirror = getattr(bus, _MARKER, None)
        if mirror is None:
            mirror = ClaudeSessionMirror(bus=bus, claude_home=claude_home)
            setattr(bus, _MARKER, mirror)
        mirror.attach()
        _ACTIVE = mirror
    return mirror


def uninstall(bus: EventBus = event_bus) -> None:
    """卸载（幂等）：撤销订阅并清掉单例标记。"""
    global _ACTIVE
    mirror = getattr(bus, _MARKER, None)
    if mirror is None:
        return
    mirror.detach()
    setattr(bus, _MARKER, None)
    if _ACTIVE is mirror:
        _ACTIVE = None


def _install_default() -> None:
    """模块装载（load_hooks exec / import）时的默认安装；失败只降级。"""
    try:
        install()
    except Exception as e:  # noqa: BLE001 — 钩子文件加载不能被它拖挂
        log.warning(f"[claude-mirror] install failed: {e}")


_install_default()
