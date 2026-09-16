"""history.jsonl 独立解析视图（design D2/D6、tasks 5.1）。

**独立实现**：本模块不 import ``wing`` —— 落盘格式只被当作"外来生产者的产物"
手写解析（design D2 的独立性判据）。被解析的格式（只读参考：

``libs/core/wing/store/file.py`` / ``common/tracked_list.py``）：

- 每行一条 JSON：Message 记录（``role ∈ system/user/assistant/tool``）与事件记录
  （``role="event"`` + ``type``）**混排**、共享链拓扑（``uuid`` / ``parent_uuid``）；
- 写盘语义是「append_detached + set_tip」：新链的最后写入节点即当前 tip，
  自 tip 沿 ``parent_uuid`` 回溯得到活跃链；被移出链的节点仍留在文件里
  （append-only）——``records`` 因此保留全量记录；
- ``unzip_last_uuid`` 是压缩节点的"被压缩区末端"指针（compact 语义的唯一锚点）；
- 损坏行（非法 JSON / 非对象行）跳过而非崩溃，与文件后端读盘行为对等。

**快照语义**：构造即读盘一次，此后 ``records`` / ``by_uuid`` 不再变化——
"操作前 / 操作后"两个视图才能被对账。需要最新状态时用 :meth:`HistoryView.reload`
取一个新视图（不修改自身）。

比较口径（见 :func:`message_semantics`）：``role`` / ``content`` /
``reasoning_content`` / ``tool_calls`` / ``tool_call_id`` / ``content_blocks``。
**不含** ``usage`` / ``stop_reason``——实现上 rewind 复制行与 compact 重链接行
不复制这两个 provider 响应审计字段，它们也不进入 LLM 请求体。
"""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any, cast

#: 文件后端布局中的文件名（``store/file.py``）。
HISTORY_FILE = "history.jsonl"
METADATA_FILE = "metadata.json"

#: Message 记录的 role 取值（事件记录恒为 ``"event"``）。
MESSAGE_ROLES: frozenset[str] = frozenset({"system", "user", "assistant", "tool"})

#: 事件记录的 role 判别值。
EVENT_ROLE = "event"

#: 压缩节点内容的语义前缀（``Compactor.do_compact`` 产出 ``"[Compact] <summary>"``）。
COMPACT_PREFIX = "[Compact]"

#: 回退到根时实现写入的哨兵节点（``ContextManager.rewind`` 的 root 分支）。
REWIND_TO_ROOT_CONTENT = "[rewind_to_root]"

#: 链拓扑 + 写入噪声字段：语义比较、事件载荷对账时剔除。
TOPOLOGY_KEYS: frozenset[str] = frozenset(
    {"uuid", "parent_uuid", "unzip_last_uuid", "ts"}
)

#: 三种 content block 的字段默认值（缺失时补齐，避免"少一个键"被当成差异；
#: 未知键保留在结果里，新字段不会被静默忽略）。
_BLOCK_DEFAULTS: dict[str, dict[str, Any]] = {
    "text": {"text": ""},
    "thinking": {"thinking": "", "signature": None, "redacted": False},
    "tool_use": {"id": "", "name": "", "input": {}, "input_error": None},
}

_MISSING: Any = object()


class HistoryAssertionError(AssertionError):
    """history 视图 / 不变量 / 红线过渡断言失败。

    报告含定位要素：记录行号、链下标、uuid、消息下标、``call_id``、
    期望 vs 实际（截断显示）——失败无需重跑即可定位。
    """


# ── 通用工具 ──────────────────────────────────────────────


def truncate(text: str, limit: int = 160) -> str:
    """超长截断（附原始长度，报告可读且不淹没上下文）。"""
    if len(text) <= limit:
        return text
    return f"{text[:limit]}…(+{len(text) - limit} chars)"


def _as_mapping(value: Any) -> Mapping[str, Any] | None:
    """dict 态检查 + 类型收窄（``Mapping`` 的键类型在 ty 下不变）。"""
    if isinstance(value, Mapping):
        return cast("Mapping[str, Any]", value)
    return None


def record_uuid(record: Mapping[str, Any]) -> str | None:
    """记录的 uuid（缺失/非字符串返回 None）。"""
    uuid = record.get("uuid")
    return uuid if isinstance(uuid, str) and uuid else None


def is_event(record: Mapping[str, Any]) -> bool:
    """是否为事件记录（``role="event"``）。"""
    return record.get("role") == EVENT_ROLE


def is_message(record: Mapping[str, Any]) -> bool:
    """是否为 Message 记录（role ∈ system/user/assistant/tool）。"""
    return record.get("role") in MESSAGE_ROLES


def iter_messages(records: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    """过滤 Message 记录（顺序保留）。"""
    return [record for record in records if is_message(record)]


def iter_events(records: Sequence[Mapping[str, Any]]) -> list[Mapping[str, Any]]:
    """过滤事件记录（顺序保留）。"""
    return [record for record in records if is_event(record)]


def record_label(record: Mapping[str, Any], *, position: int | None = None) -> str:
    """一行式记录标签：``[chain 3] message uuid=abcd1234 role=user``。"""
    kind = "event" if is_event(record) else str(record.get("role") or "unknown")
    uuid = record_uuid(record) or "<no-uuid>"
    where = f"chain {position} " if position is not None else ""
    extra = f" type={record.get('type')}" if is_event(record) else ""
    return f"{where}{kind} uuid={uuid}{extra}"


def describe_record(
    record: Mapping[str, Any],
    *,
    position: int | None = None,
    line: int | None = None,
    limit: int = 120,
) -> str:
    """失败报告用的一条记录描述（含角色 / uuid / 内容摘要 / 链定位）。"""
    parts: list[str] = []
    if line is not None:
        parts.append(f"line {line}")
    parts.append(record_label(record, position=position))
    parent = record.get("parent_uuid")
    if isinstance(parent, str) and parent:
        parts.append(f"parent={parent}")
    if is_event(record):
        for key in ("text", "content", "message", "tool_call_id", "result"):
            value = record.get(key)
            if isinstance(value, str) and value:
                parts.append(f"{key}={truncate(value, limit)!r}")
    else:
        content = record.get("content")
        if isinstance(content, str) and content:
            parts.append(f"content={truncate(content, limit)!r}")
        calls = record.get("tool_calls")
        if isinstance(calls, list) and calls:
            names: list[str] = []
            for call in calls:
                mapping = _as_mapping(call)
                names.append(str(mapping.get("name")) if mapping else "?")
            parts.append(f"tool_calls={names}")
        tool_call_id = record.get("tool_call_id")
        if isinstance(tool_call_id, str) and tool_call_id:
            parts.append(f"tool_call_id={tool_call_id}")
    return " ".join(parts)


def describe_chain(
    records: Sequence[Mapping[str, Any]], *, limit: int = 24, title: str = "chain"
) -> str:
    """链的逐节点描述（超长时保留头尾，中间折叠）。"""
    lines: list[str] = [f"{title} ({len(records)} node(s)):"]
    if len(records) <= limit:
        head: list[tuple[int, Mapping[str, Any]]] = list(enumerate(records))
        tail: list[tuple[int, Mapping[str, Any]]] = []
    else:
        half = max(limit // 2, 1)
        head = list(enumerate(records))[:half]
        tail = list(enumerate(records))[-half:]
    for index, record in head:
        lines.append(f"  [{index}] {describe_record(record, position=None)}")
    if tail:
        omitted = len(records) - len(head) - len(tail)
        lines.append(f"  … {omitted} node(s) omitted …")
        for index, record in tail:
            lines.append(f"  [{index}] {describe_record(record, position=None)}")
    return "\n".join(lines)


# ── 语义投影与差异 ────────────────────────────────────────


def _normalize_block(block: Any) -> Any:
    """content block 归一化：按 type 补默认字段，保留未知键。"""
    mapping = _as_mapping(block)
    if mapping is None:
        return block
    normalized = dict(mapping)
    for key, default in _BLOCK_DEFAULTS.get(str(mapping.get("type") or ""), {}).items():
        normalized.setdefault(key, default)
    return normalized


def _normalize_blocks(blocks: Any) -> Any:
    if isinstance(blocks, list):
        return [_normalize_block(block) for block in blocks]
    return blocks


def _normalize_tool_calls(calls: Any) -> Any:
    """tool_calls 归一化：``arguments`` 保持原样（dict / JSON 文本都不改写）。

    ``arguments`` 在历史记录里是已解析的 dict（``ToolCall.arguments``）；
    手工构造的旧形态（JSON 文本）原样保留——参数合法性由
    :func:`wing_probe.history.invariants.assert_tool_pairing` 单独断言，
    这里只做比较口径的归一化。
    """
    if not isinstance(calls, list):
        return calls
    normalized: list[Any] = []
    for call in calls:
        mapping = _as_mapping(call)
        if mapping is None:
            normalized.append(call)
            continue
        normalized.append(
            {
                "id": mapping.get("id"),
                "name": mapping.get("name"),
                "arguments": mapping.get("arguments"),
                "arguments_error": mapping.get("arguments_error"),
            }
        )
    return normalized


def message_semantics(record: Mapping[str, Any]) -> dict[str, Any]:
    """Message 记录的**上下文事实**投影（红线过渡断言的比较口径）。

    参与比较：``role`` / ``content`` / ``reasoning_content`` /
    ``tool_calls``（id/name/arguments/arguments_error）/ ``tool_call_id`` /
    ``content_blocks``（按 type 补默认字段）。

    不参与比较：``uuid`` / ``parent_uuid`` / ``unzip_last_uuid`` / ``ts``
    （链拓扑与写入噪声）、``usage`` / ``stop_reason``（provider 响应审计，
    不属于上下文事实，且实现的 rewind 复制行不复制它们）。
    """
    blocks = record.get("content_blocks")
    return {
        "role": record.get("role"),
        "content": record.get("content"),
        "reasoning_content": record.get("reasoning_content"),
        "tool_calls": _normalize_tool_calls(record.get("tool_calls")),
        "tool_call_id": record.get("tool_call_id"),
        "content_blocks": None if blocks is None else _normalize_blocks(blocks),
    }


def event_semantics(record: Mapping[str, Any]) -> dict[str, Any]:
    """事件记录的载荷投影（剔除链拓扑与 ``ts``）。"""
    return {key: value for key, value in record.items() if key not in TOPOLOGY_KEYS}


def node_semantics(record: Mapping[str, Any]) -> dict[str, Any]:
    """节点比较语义：Message → :func:`message_semantics`；事件 → 载荷投影。"""
    if is_event(record):
        return event_semantics(record)
    return message_semantics(record)


def semantic_diff(expected: Mapping[str, Any], actual: Mapping[str, Any]) -> list[str]:
    """逐字段差异（嵌套 dict 递归，报告形如 ``content: expected 'a' got 'b'``）。"""
    problems: list[str] = []
    for key in sorted(set(expected) | set(actual)):
        has_left = key in expected
        has_right = key in actual
        left = expected[key] if has_left else _MISSING
        right = actual[key] if has_right else _MISSING
        if has_left and has_right and left == right:
            continue
        left_map = _as_mapping(left)
        right_map = _as_mapping(right)
        if left_map is not None and right_map is not None:
            problems.extend(
                f"{key}.{line}" for line in semantic_diff(left_map, right_map)
            )
            continue
        problems.append(
            f"{key}: expected {truncate(repr(left))} got {truncate(repr(right))}"
        )
    return problems


# ── 视图 ──────────────────────────────────────────────────


class HistoryView:
    """``<session_dir>/history.jsonl`` 的解析视图（构造即快照）。

    ``session_dir`` 即 ``<sessions_root>/<session_id>``——下游用
    ``ProbeEnv.session_dir(session_id)`` 取得。
    """

    def __init__(self, session_dir: Path | str) -> None:
        self.session_dir: Path = Path(session_dir)
        self.path: Path = self.session_dir / HISTORY_FILE
        self.metadata_path: Path = self.session_dir / METADATA_FILE

        #: 全量原始记录（Message + 事件，文件序）；损坏行已跳过。
        self.records: list[dict[str, Any]] = []
        #: 与 ``records`` 对齐的源文件行号（1-based，定位用）。
        self.record_lines: list[int] = []
        #: uuid → 记录（同 uuid 重复时后者覆盖，镜像实现的 last-write-wins）。
        self.by_uuid: dict[str, dict[str, Any]] = {}
        #: 跳过的损坏行行号（非法 JSON / 非对象行）。
        self.corrupt_lines: list[int] = []
        #: 重复出现的 uuid → 行号列表（链拓扑不变量据此报错）。
        self.duplicate_uuids: dict[str, list[int]] = {}
        #: 解码失败原因（文件非 UTF-8 时非 None，仍尽力解析）。
        self.decode_error: str | None = None

        self._metadata_cache: dict[str, Any] | None = None
        self._metadata_loaded = False
        self._load()

    # ── 装载 ──────────────────────────────────────────────

    def _load(self) -> None:
        if not self.path.exists():
            return
        try:
            text = self.path.read_text(encoding="utf-8")
        except UnicodeDecodeError as exc:
            self.decode_error = str(exc)
            text = self.path.read_text(encoding="utf-8", errors="replace")

        for line_number, raw_line in enumerate(text.splitlines(), start=1):
            line = raw_line.strip()
            if not line:
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError:
                self.corrupt_lines.append(line_number)
                continue
            mapping = _as_mapping(record)
            if mapping is None:
                self.corrupt_lines.append(line_number)
                continue
            entry = dict(mapping)
            self.records.append(entry)
            self.record_lines.append(line_number)
            uuid = record_uuid(entry)
            if uuid is not None:
                if uuid in self.by_uuid:
                    self.duplicate_uuids.setdefault(uuid, []).append(line_number)
                self.by_uuid[uuid] = entry

    def reload(self) -> HistoryView:
        """重新读盘，返回**新视图**（自身仍保持快照语义，不被修改）。"""
        return HistoryView(self.session_dir)

    # ── 基本属性 ──────────────────────────────────────────

    @property
    def session_id(self) -> str:
        """session id（= 目录名）。"""
        return self.session_dir.name

    @property
    def exists(self) -> bool:
        """history.jsonl 是否存在（新 session 未写盘时为 False）。"""
        return self.path.exists()

    @property
    def last_record_uuid(self) -> str | None:
        """末条记录的 uuid（写盘顺序的最后一行）。"""
        for record in reversed(self.records):
            uuid = record_uuid(record)
            if uuid is not None:
                return uuid
        return None

    @property
    def tip_uuid(self) -> str | None:
        """当前 tip：文件序中最后一条"不作为任何记录 parent"的记录。

        与 ``TrackedList.trace_chain(from_uuid=None)`` 的叶节点判定同构
        （``_lines_order`` 即 append 顺序）。全部记录都被引用（异常拓扑 /
        环）时退化为末条记录。
        """
        parents: set[str] = set()
        for record in self.records:
            parent = record.get("parent_uuid")
            if isinstance(parent, str) and parent:
                parents.add(parent)
        for record in reversed(self.records):
            uuid = record_uuid(record)
            if uuid is not None and uuid not in parents:
                return uuid
        return self.last_record_uuid

    # ── 链遍历 ────────────────────────────────────────────

    def _ascend(self, start_uuid: str) -> tuple[list[dict[str, Any]], str | None]:
        """自 ``start_uuid`` 沿 ``parent_uuid`` 回溯到根。

        返回 ``(根 → start 顺序的链, 中断原因)``：中断原因非 None 表示
        parent 断裂（uuid 缺失）或成环。
        """
        nodes: list[dict[str, Any]] = []
        seen: set[str] = set()
        current: str | None = start_uuid
        while isinstance(current, str) and current:
            if current in seen:
                return list(reversed(nodes)), f"cycle at uuid {current}"
            seen.add(current)
            node = self.by_uuid.get(current)
            if node is None:
                return list(reversed(nodes)), f"dangling uuid {current}"
            nodes.append(node)
            parent = node.get("parent_uuid")
            current = parent if isinstance(parent, str) and parent else None
        return list(reversed(nodes)), None

    def chain_ending_at(self, uuid: str) -> list[dict[str, Any]]:
        """以 ``uuid`` 为叶、沿 ``parent_uuid`` 回溯到根的链（根 → uuid 顺序）。"""
        return self._ascend(uuid)[0]

    def ascend_report(self, uuid: str) -> str | None:
        """回溯中断原因（None 表示成功到达根）——失败报告素材。"""
        return self._ascend(uuid)[1]

    def active_chain(self) -> list[dict[str, Any]]:
        """活跃链：自 tip 沿 ``parent_uuid`` 回溯（根 → tip 顺序）。

        与实现「append_detached + set_tip」的写盘语义一致：rewind / compact /
        fork 之后依然成立；被移出链的节点不在结果里，但仍留在 :attr:`records`。
        """
        tip = self.tip_uuid
        if tip is None:
            return []
        return self.chain_ending_at(tip)

    def full_chain(self, from_uuid: str | None = None) -> list[dict[str, Any]]:
        """完整链（含压缩节点、跨压缩边界），镜像 ``walk_full_chain`` 语义。

        - 自 ``from_uuid``（默认 tip）向上回溯；
        - 遇到 ``parent_uuid`` 为空且带 ``unzip_last_uuid`` 的压缩节点时，
          先收录该节点，再沿 ``unzip_last_uuid`` 继续（被压缩区间因此回到链上）。
        """
        start = from_uuid if from_uuid is not None else self.tip_uuid
        nodes: list[dict[str, Any]] = []
        seen: set[str] = set()
        current: str | None = start
        while isinstance(current, str) and current:
            if current in seen:
                break
            seen.add(current)
            node = self.by_uuid.get(current)
            if node is None:
                break
            nodes.append(node)
            parent = node.get("parent_uuid")
            unzip = node.get("unzip_last_uuid")
            if isinstance(parent, str) and parent:
                current = parent
            elif isinstance(unzip, str) and unzip and unzip != current:
                current = unzip
            else:
                current = None
        nodes.reverse()
        return nodes

    # ── 分类过滤 ──────────────────────────────────────────

    def messages(self) -> list[dict[str, Any]]:
        """活跃链上的 Message 记录（顺序保留）。"""
        return [record for record in self.active_chain() if is_message(record)]

    def events(self) -> list[dict[str, Any]]:
        """活跃链上的事件记录（顺序保留）。"""
        return [record for record in self.active_chain() if is_event(record)]

    def chain_messages(
        self, chain: Sequence[Mapping[str, Any]]
    ) -> list[Mapping[str, Any]]:
        """任意链上的 Message 记录（顺序保留）。"""
        return iter_messages(chain)

    # ── 查询 ──────────────────────────────────────────────

    def find(self, uuid: str) -> dict[str, Any] | None:
        """按 uuid 取记录（全量记录，不限活跃链）。"""
        return self.by_uuid.get(uuid)

    def parent_of(self, uuid: str) -> dict[str, Any] | None:
        """取记录的 parent 节点（根 / 断裂 / 未知 uuid 返回 None）。"""
        record = self.by_uuid.get(uuid)
        if record is None:
            return None
        parent = record.get("parent_uuid")
        if not isinstance(parent, str) or not parent:
            return None
        return self.by_uuid.get(parent)

    def materialized_parents(self) -> dict[str, str]:
        """``uuid → parent_uuid`` 映射（parent 缺失的根节点不在其中）。"""
        result: dict[str, str] = {}
        for record in self.records:
            uuid = record_uuid(record)
            parent = record.get("parent_uuid")
            if uuid is not None and isinstance(parent, str) and parent:
                result[uuid] = parent
        return result

    def line_of(self, uuid: str) -> int | None:
        """记录的源文件行号（定位用）。"""
        for index, record in enumerate(self.records):
            if record_uuid(record) == uuid:
                return self.record_lines[index]
        return None

    def metadata(self) -> dict[str, Any] | None:
        """``metadata.json``（缺失返回 None，损坏抛 :class:`HistoryAssertionError`）。"""
        if not self._metadata_loaded:
            self._metadata_cache = read_metadata(self.session_dir)
            self._metadata_loaded = True
        return self._metadata_cache

    # ── 报告 ──────────────────────────────────────────────

    def describe(self, *, limit: int = 24) -> str:
        """视图摘要（失败报告的上下文段落）。"""
        chain = self.active_chain()
        lines = [
            f"session {self.session_id}: {len(self.records)} record(s) "
            f"({len(self.messages())} message(s), {len(self.events())} event(s) "
            f"on the active chain), tip={self.tip_uuid}",
            f"  history: {self.path}",
        ]
        if self.corrupt_lines:
            lines.append(f"  corrupt lines skipped: {self.corrupt_lines}")
        if self.decode_error:
            lines.append(f"  decode error: {self.decode_error}")
        lines.append(describe_chain(chain, limit=limit))
        return "\n".join(lines)

    def _failure(self, title: str, problems: Sequence[str]) -> str:
        body = "\n".join(f"  - {problem}" for problem in problems)
        return (
            f"{title} ({len(problems)} problem(s)) [session {self.session_id}]:\n"
            f"{body}\n{self.describe()}"
        )

    # ── 不变量（实现见 history/invariants.py） ────────────

    def assert_chain_invariants(self) -> None:
        """链拓扑不变量（uuid 唯一、parent 可达根、tip 自洽）。"""
        from wing_probe.history.invariants import assert_chain_invariants

        assert_chain_invariants(self)

    def assert_tool_pairing(self, *, allow_arguments_error: bool = False) -> None:
        """活跃链上的 tool 配对不变量（无缺失、无孤儿、参数完整）。"""
        from wing_probe.history.invariants import assert_tool_pairing

        assert_tool_pairing(self, allow_arguments_error=allow_arguments_error)

    def assert_no_transient_records(
        self, *, types: Sequence[str] | None = None
    ) -> None:
        """流式 delta 类瞬态记录不得落盘（上下文事实口径）。"""
        from wing_probe.history.invariants import assert_no_transient_records

        assert_no_transient_records(self, types=types)

    def __repr__(self) -> str:
        return (
            f"HistoryView(session_id={self.session_id!r}, "
            f"records={len(self.records)}, tip={self.tip_uuid!r})"
        )


# ── metadata.json 独立解析 ────────────────────────────────


def read_metadata(session_dir: Path | str) -> dict[str, Any] | None:
    """独立解析 ``<session_dir>/metadata.json``（缺失返回 None）。

    损坏 JSON 抛 :class:`HistoryAssertionError`：实现会"warning + 当作空
    metadata + 下次保存覆盖"，静默丢失 ``forked_from`` / 模型快照——探针
    不吞掉这个信号（与"存储完整性归后端管"的容忍口径相反，这里正是要抓）。
    """
    path = Path(session_dir) / METADATA_FILE
    if not path.exists():
        return None
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (json.JSONDecodeError, UnicodeDecodeError, OSError) as exc:
        raise HistoryAssertionError(
            f"corrupted metadata.json at {path}: {exc}"
        ) from exc
    mapping = _as_mapping(data)
    if mapping is None:
        raise HistoryAssertionError(
            f"metadata.json at {path} is not a JSON object: {type(data).__name__}"
        )
    return dict(mapping)


__all__ = [
    "COMPACT_PREFIX",
    "EVENT_ROLE",
    "HISTORY_FILE",
    "METADATA_FILE",
    "MESSAGE_ROLES",
    "REWIND_TO_ROOT_CONTENT",
    "TOPOLOGY_KEYS",
    "HistoryAssertionError",
    "HistoryView",
    "describe_chain",
    "describe_record",
    "event_semantics",
    "is_event",
    "is_message",
    "iter_events",
    "iter_messages",
    "message_semantics",
    "node_semantics",
    "read_metadata",
    "record_label",
    "record_uuid",
    "semantic_diff",
    "truncate",
]
