"""上下文不变量与红线过渡断言（design D5/D6、spec probe-context-invariants、tasks 5.2/5.3）。

三层内容：

1. **内置不变量**（每个场景结束由 fixture 自动运行，见 tasks 6.1）：
   :func:`assert_chain_invariants` / :func:`assert_tool_pairing` /
   :func:`assert_no_transient_records`；
2. **红线过渡断言**（compact / rewind / fork 的前后对账）：
   :func:`assert_compact_transition` / :func:`assert_rewind_transition` /
   :func:`assert_fork_of`；
3. **metadata 快照**：:func:`fork_metadata`（``metadata.json`` 独立解析 + 必需字段）。

口径（用户决策，design D5）：只保 ``history`` 链上的**上下文事实**——链拓扑、
记录配对、瞬态记录不落盘；**不做**"全事件类型 × persist 布尔矩阵"对账（二批）。

比较口径的**事实依据**（读产品源码确认，不在本包 import）：

- compact（手动 ``do_manual_compact``）：新链 = 压缩节点（``role="assistant"``、
  ``content`` 以 ``[Compact] `` 开头、``parent_uuid=None``、
  ``unzip_last_uuid`` == 被压缩区末端 Message 的 uuid）；压缩区之后没有保留区
  （整窗压缩）。runtime 随后落一条 ``compact_done`` 事实事件（``persist=True``）
  在压缩节点之后——因此本模块允许压缩节点之后出现**事件节点**，但**不接受**
  计划外的 Message。
- compact（后台 apply ``_apply_pending_compact``）：新链 = 压缩节点 + 重链接 tail
  （被压缩区之后的 Message 全部换新 uuid、``parent_uuid`` 重接）。
- rewind：新 tip 是"复制行"——复制 **target 的最近 Message 祖先**（跳过事件节点）
  的 role/content/reasoning/tool_calls/tool_call_id/content_blocks，全新 uuid，
  ``parent_uuid`` 指向祖父（可能是事件节点）；target 及其后续不在活跃链但仍在
  记录集。target 上方只有事件（或无节点）时，实现写入哨兵节点
  ``role="system"`` / ``content="[rewind_to_root]"``（parent 为空）。
- fork：子链 == ``walk_full_chain(from_uuid=at.parent_uuid)`` 的重映射副本
  （``uuid`` / ``parent_uuid`` / ``unzip_last_uuid`` 全量重映射、事件随行）；
  ``metadata.json`` 一次写全 ``forked_from`` / ``workspace`` / ``template_name`` /
  ``model_name`` / ``provider_name``（模型快照）。
"""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import Any

from wing_probe.history.view import (
    COMPACT_PREFIX,
    MESSAGE_ROLES,
    REWIND_TO_ROOT_CONTENT,
    HistoryAssertionError,
    HistoryView,
    describe_chain,
    describe_record,
    is_event,
    is_message,
    node_semantics,
    read_metadata,
    record_uuid,
    semantic_diff,
    truncate,
)

#: spec 硬要求的最小黑名单：流式 delta 形态（正文 / 推理 / 工具参数增量）。
DELTA_EVENT_TYPES: frozenset[str] = frozenset({"text", "reasoning", "tool_call_stream"})

#: 扩展黑名单：产品中 ``persist=False`` 的全部事件类型（只读参考
#: ``libs/core/wing/event/{react,state_change,query_response,base}.py``）。
#: ``persist=False`` 的语义是"只广播、不落盘、无 Message 孪生"——出现在
#: ``history.jsonl`` 里即为持久化语义被破坏（比 spec 的最低要求更严）。
TRANSIENT_EVENT_TYPES: frozenset[str] = DELTA_EVENT_TYPES | frozenset(
    {
        # event/react.py
        "tool_call",
        "tool_call_result",
        "llm_call_metrics",
        "assistant_turn",
        "tool_result_turn",
        # event/state_change.py
        "sync_session",
        "session_state_changed",
        "session_init",
        # event/query_response.py
        "context_stats",
        "branch_targets",
        # event/base.py
        "delivered",
        "notice",
    }
)

#: fork 的 metadata 快照必需字段（spec「fork 断言」：缺一项即失败）。
FORK_METADATA_FIELDS: tuple[str, ...] = (
    "forked_from",
    "workspace",
    "template_name",
    "model_name",
    "provider_name",
)

#: 与源 session 的**当前** metadata 交叉对账的快照字段（`forked_from` 恒校验）。
FORK_SNAPSHOT_FIELDS: tuple[str, ...] = (
    "workspace",
    "template_name",
    "model_name",
    "provider_name",
)

#: rewind / fork 的"当前状态"哨兵（实现里 target == "current" 有专属语义）。
CURRENT_SENTINEL = "current"


# ── 报告 ─────────────────────────────────────────────────


def _failure(
    title: str,
    problems: Sequence[str],
    *,
    sections: Sequence[str] = (),
    session_id: str | None = None,
) -> str:
    """统一失败报告：标题 + 问题清单 + 现场片段。"""
    where = f" [session {session_id}]" if session_id else ""
    body = "\n".join(f"  - {problem}" for problem in problems)
    parts = [f"{title}{where} ({len(problems)} problem(s)):", body]
    parts.extend(sections)
    return "\n".join(parts)


def _record_ref(
    record: Mapping[str, Any], *, position: int | None = None, line: int | None = None
) -> str:
    return describe_record(record, position=position, line=line)


# ── 不变量 1：链拓扑 ──────────────────────────────────────


def assert_chain_invariants(view: HistoryView) -> None:
    """链拓扑不变量（tasks 5.2）。

    断言（全部在**记录集**范围内，不止活跃链）：

    - 每条记录都有 uuid，且 uuid 唯一；
    - 每条记录是 Message（role ∈ system/user/assistant/tool）或事件
      （``role="event"`` + 非空 ``type``）——否则读盘时会被跳过，链条静默断裂；
    - 每条记录的 ``parent_uuid`` 要么为空（根），要么在记录集中存在；
    - 活跃链自 tip 回溯可达根、无环，且逐节点 parent 衔接（链是闭合路径）；
    - 活跃链末端 == tip == 末条记录（写盘语义"末条记录即 tip"自洽）。

    Raises:
        HistoryAssertionError: 任一断言失败，报告含行号 / uuid / 链下标定位。
    """
    problems: list[str] = []
    first_seen: dict[str, int] = {}

    for index, record in enumerate(view.records):
        line = view.record_lines[index]
        uuid = record_uuid(record)
        if uuid is None:
            problems.append(
                f"line {line}: missing uuid ({_record_ref(record, line=line)})"
            )
            continue
        if uuid in first_seen:
            problems.append(
                f"line {line}: duplicated uuid {uuid!r} "
                f"(first seen at line {first_seen[uuid]})"
            )
        else:
            first_seen[uuid] = line

        role = record.get("role")
        if is_event(record):
            event_type = record.get("type")
            if not isinstance(event_type, str) or not event_type:
                problems.append(
                    f"line {line}: event record without a usable 'type' "
                    f"({_record_ref(record, line=line)})——读盘时事件类型未知会被跳过，"
                    "活跃链在此静默断裂"
                )
        elif not is_message(record):
            problems.append(
                f"line {line}: unknown record kind role={role!r} "
                f"({_record_ref(record, line=line)})——既不匹配 Message role "
                f"{sorted(MESSAGE_ROLES)} 也不是 role='event'，读盘时会被跳过"
            )

        parent = record.get("parent_uuid")
        if isinstance(parent, str) and parent and parent not in view.by_uuid:
            problems.append(
                f"line {line}: parent_uuid={parent} is not in the record set "
                f"({_record_ref(record, line=line)})"
            )

    tip = view.tip_uuid
    chain = view.active_chain()
    if tip is None:
        if view.records:
            problems.append(
                f"{len(view.records)} record(s) but no tip could be derived "
                "(every record is referenced as a parent —— 环或异常拓扑)"
            )
    else:
        # 环检测（独立于链闭包：环会让链闭包"看起来"自洽）
        seen: set[str] = set()
        cursor: str | None = tip
        while isinstance(cursor, str) and cursor:
            if cursor in seen:
                problems.append(
                    f"cycle detected while walking up from tip {tip!r}: "
                    f"uuid {cursor!r} revisited (chain length before cycle: {len(seen)})"
                )
                break
            seen.add(cursor)
            node = view.by_uuid.get(cursor)
            if node is None:
                break
            parent = node.get("parent_uuid")
            cursor = parent if isinstance(parent, str) and parent else None

        for position, node in enumerate(chain):
            parent = node.get("parent_uuid")
            line = view.line_of(record_uuid(node) or "")
            if position == 0:
                if isinstance(parent, str) and parent:
                    problems.append(
                        f"chain[0] {_record_ref(node, position=0, line=line)}: "
                        f"parent_uuid={parent} is not in the record set —— "
                        "活跃链不可达根（回溯中断）"
                    )
                continue
            expected_parent = record_uuid(chain[position - 1])
            if parent != expected_parent:
                problems.append(
                    f"chain[{position}] {_record_ref(node, position=position, line=line)}: "
                    f"parent_uuid={parent} != chain[{position - 1}].uuid="
                    f"{expected_parent} —— 链不是闭合路径"
                )

        if chain and record_uuid(chain[-1]) != tip:
            problems.append(
                f"chain end uuid={record_uuid(chain[-1])} != tip={tip} —— "
                "回溯起点与叶节点判定不一致"
            )
        last = view.last_record_uuid
        if last != tip:
            problems.append(
                f"tip={tip!r} != last record uuid={last!r} "
                f"(line {view.record_lines[-1] if view.record_lines else '?'}) —— "
                "写盘语义是「末条记录即 tip」，不一致意味着写盘顺序异常"
            )

    if problems:
        raise HistoryAssertionError(
            _failure(
                "chain topology violated",
                problems,
                sections=[view.describe()],
                session_id=view.session_id,
            )
        )


# ── 不变量 2：tool 配对 ───────────────────────────────────


def _arguments_problems(
    *,
    position: int,
    record: Mapping[str, Any],
    call_id: str,
    name: str,
    call: Mapping[str, Any],
    allow_arguments_error: bool,
) -> list[str]:
    problems: list[str] = []
    where = (
        f"chain[{position}] {_record_ref(record, position=position)} "
        f"call {call_id!r} ({name})"
    )
    if "arguments" not in call:
        problems.append(f"{where}: missing 'arguments' field (工具参数缺失/半截块)")
    arguments = call.get("arguments")
    if isinstance(arguments, str):
        try:
            parsed = json.loads(arguments)
        except ValueError as exc:
            problems.append(
                f"{where}: arguments are not valid JSON "
                f"({truncate(arguments, 200)!r}; {exc}) —— 半截参数不得落盘"
            )
        else:
            if not isinstance(parsed, dict):
                problems.append(
                    f"{where}: arguments decode to {type(parsed).__name__}, "
                    f"expected a JSON object ({truncate(arguments, 200)!r})"
                )
    elif arguments is None or not isinstance(arguments, Mapping):
        if arguments is not None:
            problems.append(
                f"{where}: arguments has unexpected type "
                f"{type(arguments).__name__} ({truncate(repr(arguments), 120)})"
            )
    error = call.get("arguments_error")
    if isinstance(error, str) and error and not allow_arguments_error:
        problems.append(
            f"{where}: arguments_error recorded ({truncate(error, 160)!r}) —— "
            "参数解析失败的调用已落链；若非预期的「坏参数」场景，"
            "用 assert_tool_pairing(allow_arguments_error=True) 复核"
        )
    return problems


def assert_tool_pairing(
    view: HistoryView, *, allow_arguments_error: bool = False
) -> None:
    """活跃链上的 tool 配对不变量（tasks 5.2）。

    断言：每个 ``assistant.tool_calls`` 的 call_id 都被**其后**的 tool 消息配对；
    没有孤儿 tool 消息（``tool_call_id`` 无前序 assistant tool_call）；没有重复
    call_id / 重复 tool 结果；每个 tool_call 的参数是完整合法 JSON 对象
    （无半截块）；``arguments_error`` 默认视为违规（见 ``allow_arguments_error``）。

    Args:
        allow_arguments_error: 放开 ``arguments_error`` 检查——仅用于**有意**
            构造"模型吐出坏参数"场景的对账；默认关闭（保持红线口径）。

    Raises:
        HistoryAssertionError: 任一断言失败，报告含链下标 / uuid / call_id。
    """
    chain = view.active_chain()
    problems: list[str] = []
    calls: dict[str, tuple[int, str]] = {}
    paired: dict[str, int] = {}

    for position, record in enumerate(chain):
        if not is_message(record):
            continue
        role = record.get("role")
        if role == "assistant":
            raw_calls = record.get("tool_calls")
            if raw_calls is not None and not isinstance(raw_calls, list):
                problems.append(
                    f"chain[{position}] {_record_ref(record, position=position)}: "
                    f"tool_calls is not a list ({type(raw_calls).__name__})"
                )
                continue
            for call_position, raw_call in enumerate(raw_calls or []):
                if not isinstance(raw_call, Mapping):
                    problems.append(
                        f"chain[{position}] {_record_ref(record, position=position)} "
                        f"tool_calls[{call_position}]: not an object "
                        f"({type(raw_call).__name__})"
                    )
                    continue
                call_id = raw_call.get("id")
                name = str(raw_call.get("name") or "")
                if not isinstance(call_id, str) or not call_id:
                    problems.append(
                        f"chain[{position}] {_record_ref(record, position=position)} "
                        f"tool_calls[{call_position}] ({name}): missing call id "
                        "—— 无 id 的调用无法被配对"
                    )
                    continue
                if call_id in calls:
                    problems.append(
                        f"chain[{position}] {_record_ref(record, position=position)} "
                        f"call {call_id!r}: duplicated call_id "
                        f"(first at chain[{calls[call_id][0]}])"
                    )
                    continue
                calls[call_id] = (position, name)
                problems.extend(
                    _arguments_problems(
                        position=position,
                        record=record,
                        call_id=call_id,
                        name=name,
                        call=raw_call,
                        allow_arguments_error=allow_arguments_error,
                    )
                )
        elif role == "tool":
            call_id = record.get("tool_call_id")
            if not isinstance(call_id, str) or not call_id:
                problems.append(
                    f"chain[{position}] {_record_ref(record, position=position)}: "
                    "tool message without tool_call_id"
                )
            elif call_id not in calls:
                problems.append(
                    f"chain[{position}] {_record_ref(record, position=position)} "
                    f"tool_call_id={call_id!r}: orphan "
                    "(无前序 assistant tool_call)"
                )
            elif call_id in paired:
                problems.append(
                    f"chain[{position}] {_record_ref(record, position=position)} "
                    f"tool_call_id={call_id!r}: duplicate tool result "
                    f"(already paired at chain[{paired[call_id]}])"
                )
            else:
                paired[call_id] = position

    for call_id, (position, name) in calls.items():
        if call_id not in paired:
            record = chain[position]
            problems.append(
                f"chain[{position}] {_record_ref(record, position=position)} "
                f"call {call_id!r} ({name}): missing paired tool message "
                f"with tool_call_id={call_id!r}"
            )

    if problems:
        raise HistoryAssertionError(
            _failure(
                "tool pairing violated",
                problems,
                sections=[describe_chain(chain)],
                session_id=view.session_id,
            )
        )


# ── 不变量 3：瞬态记录不落盘 ──────────────────────────────


def assert_no_transient_records(
    view: HistoryView, *, types: Sequence[str] | None = None
) -> None:
    """流式 delta / 瞬态事件不得出现在 ``history.jsonl``（tasks 5.2）。

    ``types`` 默认 :data:`TRANSIENT_EVENT_TYPES`（产品 ``persist=False`` 全集的
    超集，含 spec 最低要求的 ``text`` / ``reasoning`` / ``tool_call_stream``）。

    口径（比 spec 更严）：spec 要求"活跃链上 MUST NOT 出现"，本函数检查**全量
    记录**——瞬态记录一旦落盘（无论之后是否被 rewind / compact 移出链）都是
    持久化语义被破坏，不该因为后续操作而"看不见"。

    Raises:
        HistoryAssertionError: 命中的每条记录都给出行号 / uuid / type / 是否在链上。
    """
    blacklist = frozenset(types) if types is not None else TRANSIENT_EVENT_TYPES
    chain_uuids = {record_uuid(record) for record in view.active_chain()}
    problems: list[str] = []
    for index, record in enumerate(view.records):
        if not is_event(record):
            continue
        event_type = record.get("type")
        if event_type not in blacklist:
            continue
        line = view.record_lines[index]
        uuid = record_uuid(record)
        where = "on active chain" if uuid in chain_uuids else "detached (链外)"
        problems.append(
            f"line {line}: transient record type={event_type!r} {where} "
            f"({_record_ref(record, line=line)}) —— persist=False 的瞬态事件"
            "只广播不落盘"
        )

    if problems:
        raise HistoryAssertionError(
            _failure(
                "transient (streaming delta) records persisted",
                problems,
                sections=[view.describe()],
                session_id=view.session_id,
            )
        )


# ── 过渡断言的公共零件 ────────────────────────────────────


def _semantic_prefix_problems(
    expected: Sequence[Mapping[str, Any]],
    actual: Sequence[Mapping[str, Any]],
    *,
    expected_label: str,
    actual_label: str,
    allow_trailing: bool,
) -> tuple[list[str], list[Mapping[str, Any]]]:
    """逐个节点对账 ``actual`` 前缀 vs ``expected``，返回 (问题, 尾部多余节点)。

    - 缺节点 / 长度不足：报出缺少的期望节点；
    - 语义差异：逐字段给出 ``expected … got …``（Message 比对上下文事实，
      事件比对载荷）；
    - 尾部多余节点：事件节点容许（compact 之后 runtime 会落 ``compact_done``
      事实事件），Message 节点默认不容许（``allow_trailing=True`` 放行，
      用于"断言发生在后续演进之后"的场景）。
    """
    problems: list[str] = []
    if len(actual) < len(expected):
        missing = expected[len(actual) :]
        problems.append(
            f"{actual_label} is shorter than {expected_label}: "
            f"expected {len(expected)} node(s), got {len(actual)}; "
            f"missing {len(missing)} node(s): "
            + "; ".join(
                describe_record(record, position=len(actual) + offset)
                for offset, record in enumerate(missing)
            )
        )
    for position in range(min(len(expected), len(actual))):
        want = expected[position]
        got = actual[position]
        diff = semantic_diff(node_semantics(want), node_semantics(got))
        for line in diff:
            problems.append(
                f"{actual_label}[{position}] "
                f"({describe_record(got, position=position)}): "
                f"differs from {expected_label}[{position}] "
                f"({describe_record(want, position=position)}): {line}"
            )
    trailing = list(actual[len(expected) :])
    for offset, record in enumerate(trailing):
        position = len(expected) + offset
        if is_message(record) and not allow_trailing:
            problems.append(
                f"{actual_label}[{position}] unexpected extra Message node "
                f"({describe_record(record, position=position)})——"
                "计划外的新消息（若断言发生在后续演进之后，"
                "用 allow_trailing=True 明确放行）"
            )
    return problems, trailing


def _freshness_problems(
    *,
    actual: Sequence[Mapping[str, Any]],
    forbidden: set[str],
    forbidden_label: str,
    actual_label: str,
) -> list[str]:
    """uuid 新鲜度：``actual`` 中每个 uuid 都不得与 ``forbidden`` 相交。"""
    problems: list[str] = []
    for position, record in enumerate(actual):
        uuid = record_uuid(record)
        if uuid is not None and uuid in forbidden:
            problems.append(
                f"{actual_label}[{position}] "
                f"({describe_record(record, position=position)}): "
                f"reuses uuid {uuid!r} already present in {forbidden_label} —— "
                "新链必须使用全新 uuid（旧 uuid 复用会让 resume / 重放把两条"
                "不同语义的记录当成同一条）"
            )
    return problems


# ── 过渡断言 1：compact ───────────────────────────────────


def assert_compact_transition(
    before: HistoryView,
    after: HistoryView,
    *,
    compact_uuid: str | None = None,
    allow_trailing: bool = False,
) -> dict[str, Any]:
    """compact 前后链形状对账（spec「compact 过渡断言」、tasks 5.3）。

    断言（``after`` 须是 compact 完成后、后续演进前的快照）：

    - 压缩节点：``after`` 活跃链根节点（``parent_uuid`` 为空）、``role="assistant"``、
      ``content`` 以 ``[Compact] `` 开头、``unzip_last_uuid`` == ``before`` 活跃链
      中被压缩区末端的 Message uuid（该 uuid 必须在 ``before`` 记录集里）；
    - 保留区：``before`` 中被压缩区之后的 Message 在 ``after`` 里逐条语义等价
      （role / content / reasoning / tool_calls / tool_call_id / content_blocks），
      且 uuid 全新（不得复用 ``before`` 的任何 uuid），parent 依次重接；
    - 旧节点不删：``before`` 的**全部** uuid 仍存在于 ``after.records``
      （compact 是 append-only）；
    - tip 自洽：``after`` 末条记录 == tip；压缩节点之后允许出现**事件节点**
      （如 runtime 的 ``compact_done`` 事实事件）与（``allow_trailing=True`` 时）
      后续演进的新 Message。

    Args:
        compact_uuid: 显式指定压缩节点 uuid（默认按"新出现的带
            ``unzip_last_uuid`` 节点"自动定位）。
        allow_trailing: 放行压缩节点 / 保留区之后的**计划外 Message**
            （仅当断言发生在后续轮次之后；默认严格）。

    Returns:
        对账素材字典（``compact_uuid`` / ``unzip_last_uuid`` /
        ``compressed_messages`` / ``tail_uuids`` / ``tail_source_uuids`` /
        ``trailing_uuids`` / ``after_chain_uuids``）。

    Raises:
        HistoryAssertionError: 任一断言失败（报告含前后链逐节点定位）。
    """
    before_chain = before.active_chain()
    after_chain = after.active_chain()
    before_uuids = set(before.by_uuid)
    problems: list[str] = []

    # ── 定位压缩节点 ──
    if compact_uuid is not None:
        candidates = [
            index
            for index, record in enumerate(after_chain)
            if record_uuid(record) == compact_uuid
        ]
        if not candidates:
            raise HistoryAssertionError(
                _failure(
                    "compact transition violated",
                    [
                        f"compact_uuid={compact_uuid!r} is not on the after active "
                        f"chain (chain uuids: "
                        f"{[record_uuid(r) for r in after_chain]})"
                    ],
                    sections=[describe_chain(after_chain, title="after chain")],
                    session_id=after.session_id,
                )
            )
    else:
        candidates = [
            index
            for index, record in enumerate(after_chain)
            if record_uuid(record) not in before_uuids
            and isinstance(record.get("unzip_last_uuid"), str)
            and record.get("unzip_last_uuid")
        ]
    if not candidates:
        raise HistoryAssertionError(
            _failure(
                "compact transition violated",
                [
                    "no compact node found on the after active chain "
                    "(expected a node absent from before, carrying unzip_last_uuid)",
                    f"before chain uuids: {[record_uuid(r) for r in before_chain]}",
                    f"after chain uuids: {[record_uuid(r) for r in after_chain]}",
                ],
                sections=[
                    describe_chain(before_chain, title="before chain"),
                    describe_chain(after_chain, title="after chain"),
                ],
                session_id=after.session_id,
            )
        )
    if len(candidates) > 1:
        raise HistoryAssertionError(
            _failure(
                "compact transition violated",
                [
                    "multiple candidate compact nodes on the after active chain: "
                    + ", ".join(
                        describe_record(after_chain[index], position=index)
                        for index in candidates
                    )
                ],
                sections=[describe_chain(after_chain, title="after chain")],
                session_id=after.session_id,
            )
        )

    compact_position = candidates[0]
    compact_node = after_chain[compact_position]
    compact_uuid = record_uuid(compact_node)
    unzip_uuid = compact_node.get("unzip_last_uuid")

    # ── 压缩节点自身形状 ──
    if compact_position != 0:
        problems.append(
            f"compact node is not the root of the after active chain "
            f"(position {compact_position}): "
            f"{describe_record(compact_node, position=compact_position)} —— "
            "压缩节点是一条新链的起点（parent_uuid 为空）"
        )
    if compact_node.get("parent_uuid"):
        problems.append(
            f"compact node must have no parent_uuid, got "
            f"{compact_node.get('parent_uuid')!r} "
            f"({describe_record(compact_node, position=compact_position)})"
        )
    if compact_node.get("role") != "assistant":
        problems.append(
            f"compact node role must be 'assistant', got "
            f"{compact_node.get('role')!r} "
            f"({describe_record(compact_node, position=compact_position)})"
        )
    compact_content = compact_node.get("content")
    if not isinstance(compact_content, str) or not compact_content.startswith(
        COMPACT_PREFIX
    ):
        problems.append(
            f"compact node content must start with {COMPACT_PREFIX!r}, got "
            f"{truncate(str(compact_content), 200)!r} "
            f"({describe_record(compact_node, position=compact_position)})"
        )

    # ── unzip_last_uuid → before 压缩区末端 ──
    before_messages = before.messages()
    tail_source: list[Mapping[str, Any]] = []
    compressed_messages = 0
    tail_resolved = False
    if not isinstance(unzip_uuid, str) or not unzip_uuid:
        problems.append(
            f"compact node has no unzip_last_uuid "
            f"({describe_record(compact_node, position=compact_position)})——"
            "压缩节点必须指向被压缩区末端的 Message uuid"
        )
    else:
        unzip_record = before.by_uuid.get(unzip_uuid)
        if unzip_record is None:
            problems.append(
                f"unzip_last_uuid={unzip_uuid!r} is not in the before record set "
                f"({len(before.records)} record(s); before chain uuids: "
                f"{[record_uuid(r) for r in before_chain]})"
            )
        elif is_event(unzip_record):
            problems.append(
                f"unzip_last_uuid={unzip_uuid!r} points to an event record, "
                "not a Message "
                f"({describe_record(unzip_record, line=before.line_of(unzip_uuid))})"
            )
        else:
            indexes = [
                index
                for index, record in enumerate(before_messages)
                if record_uuid(record) == unzip_uuid
            ]
            if not indexes:
                problems.append(
                    f"unzip_last_uuid={unzip_uuid!r} is not a Message on the before "
                    f"active chain (before messages: "
                    f"{[record_uuid(r) for r in before_messages]})——"
                    "「操作前视图」须是 compact 前的状态"
                )
            else:
                compressed_messages = indexes[0] + 1
                tail_source = list(before_messages[compressed_messages:])
                tail_resolved = True

    # ── 保留区（压缩区之后的 Message）重链接对账 ──
    after_after = after_chain[compact_position + 1 :]
    tail_actual = [record for record in after_after if is_message(record)]
    tail_uuids: list[str | None] = [record_uuid(record) for record in tail_actual]
    trailing: list[Mapping[str, Any]] = []
    if tail_resolved:
        tail_problems, _extra_messages = _semantic_prefix_problems(
            tail_source,
            tail_actual,
            expected_label="before 压缩区之后的 Message",
            actual_label="after relinked tail",
            allow_trailing=allow_trailing,
        )
        problems.extend(tail_problems)
        # 重链接区的 uuid 必须全新
        problems.extend(
            _freshness_problems(
                actual=tail_actual[: len(tail_source)],
                forbidden=before_uuids,
                forbidden_label="before records",
                actual_label="after relinked tail",
            )
        )
        # parent 依次重接（压缩节点 → tail[0] → tail[1] …）
        previous_uuid = compact_uuid
        for offset, record in enumerate(tail_actual[: len(tail_source)]):
            parent = record.get("parent_uuid")
            if parent != previous_uuid:
                problems.append(
                    f"after relinked tail[{offset}] "
                    f"({describe_record(record, position=compact_position + 1 + offset)}): "
                    f"parent_uuid={parent!r} != expected {previous_uuid!r} —— "
                    "保留区必须依次重接到压缩节点之后"
                )
            previous_uuid = record_uuid(record)
        # 尾部多余节点 = 保留区之外的节点（事件事实 / 后续演进的新 Message）
        matched = 0
        for record in after_after:
            if is_message(record) and matched < len(tail_source):
                matched += 1
                continue
            trailing.append(record)
    else:
        trailing = list(after_after)
        for offset, record in enumerate(after_after):
            if is_message(record) and not allow_trailing:
                problems.append(
                    f"after chain[{compact_position + 1 + offset}] unexpected extra "
                    f"Message node "
                    f"({describe_record(record, position=compact_position + 1 + offset)})"
                    "——压缩区末端未知，链上不得出现无法对账的 Message"
                )

    # ── 旧节点不删 ──
    removed = sorted(uuid for uuid in before_uuids if uuid not in after.by_uuid)
    if removed:
        problems.append(
            f"{len(removed)} node(s) from before disappeared from after.records "
            f"(compact 是 append-only)：{removed}"
        )

    # ── 新链不得复用旧 uuid / tip 自洽 ──
    problems.extend(
        _freshness_problems(
            actual=after_chain[: compact_position + 1],
            forbidden=before_uuids,
            forbidden_label="before records",
            actual_label="after chain",
        )
    )
    tip = after.tip_uuid
    if tip != after.last_record_uuid:
        problems.append(
            f"tip={tip!r} != last record uuid={after.last_record_uuid!r} —— "
            "写盘语义是「末条记录即 tip」"
        )
    if len(after_chain) < compact_position + 1 + len(tail_source):
        problems.append(
            f"after active chain is shorter than compact node + relinked tail: "
            f"expected at least {compact_position + 1 + len(tail_source)} node(s), "
            f"got {len(after_chain)}"
        )

    if problems:
        raise HistoryAssertionError(
            _failure(
                "compact transition violated",
                problems,
                sections=[
                    describe_chain(before_chain, title="before chain"),
                    describe_chain(after_chain, title="after chain"),
                ],
                session_id=after.session_id,
            )
        )

    return {
        "compact_uuid": compact_uuid,
        "unzip_last_uuid": unzip_uuid,
        "compressed_messages": compressed_messages,
        "tail_uuids": tail_uuids,
        "tail_source_uuids": [record_uuid(record) for record in tail_source],
        "trailing_uuids": [record_uuid(record) for record in trailing],
        "after_chain_uuids": [record_uuid(record) for record in after_chain],
    }


# ── 过渡断言 2：rewind ────────────────────────────────────


def assert_rewind_transition(
    before: HistoryView,
    after: HistoryView,
    target_uuid: str,
    *,
    allow_trailing: bool = False,
) -> dict[str, Any]:
    """rewind 前后链形状对账（spec「rewind 过渡断言」、tasks 5.3）。

    断言（``after`` 须是 rewind 完成后、后续演进前的快照）：

    - 新 tip 是**复制行**：其 role / content / reasoning_content / tool_calls /
      tool_call_id / content_blocks 与 ``before`` 中 target 的**最近 Message
      祖先**（跳过事件节点）等价；``uuid`` 全新；``parent_uuid`` == 祖父
      （可能是事件节点）；
    - 越过事件节点：target 的 parent 链上有事件节点时，复制源是最近 Message；
    - target 上方只有事件（或无节点）时，新 tip 是哨兵节点
      ``role="system"`` / ``content="[rewind_to_root]"`` / parent 为空；
    - ``target_uuid == "current"``：无操作语义——活跃链零变更；
    - target 及其后续节点不在 ``after`` 活跃链上，但仍存在于 ``after.records``；
    - 复制行之前的前缀链与 ``before`` 完全一致（uuid 逐项相同，rewind 不改写历史）。

    Args:
        target_uuid: rewind 的目标消息 uuid（或哨兵 ``"current"``）。
        allow_trailing: 放行复制行之后的**计划外 Message**（默认严格）。

    Returns:
        对账素材字典（``mode`` ∈ ``copy`` / ``root`` / ``noop``、``copy_uuid``、
        ``source_uuid``（复制源 Message 的 uuid）、``target_uuid``、
        ``expected_draft``（API 的 ``draft`` 应等于 target 的 content）、
        ``dropped_uuids``、``skipped_event_uuids``）。

    Raises:
        HistoryAssertionError: 任一断言失败。
    """
    before_chain = before.active_chain()
    after_chain = after.active_chain()
    before_uuids = set(before.by_uuid)

    # ── 哨兵：current（无操作） ──
    if target_uuid == CURRENT_SENTINEL:
        before_uuids_seq = [record_uuid(record) for record in before_chain]
        after_uuids_seq = [record_uuid(record) for record in after_chain]
        if before_uuids_seq != after_uuids_seq:
            raise HistoryAssertionError(
                _failure(
                    "rewind transition violated (target='current' 应无操作)",
                    [
                        f"active chain changed: before={before_uuids_seq} "
                        f"after={after_uuids_seq}"
                    ],
                    sections=[
                        describe_chain(before_chain, title="before chain"),
                        describe_chain(after_chain, title="after chain"),
                    ],
                    session_id=after.session_id,
                )
            )
        return {
            "mode": "noop",
            "copy_uuid": None,
            "source_uuid": None,
            "target_uuid": target_uuid,
            "expected_draft": None,
            "dropped_uuids": [],
            "skipped_event_uuids": [],
        }

    # ── 定位 target 与最近 Message 祖先（跳过事件节点） ──
    target = before.by_uuid.get(target_uuid)
    if target is None:
        raise HistoryAssertionError(
            _failure(
                "rewind transition violated",
                [
                    f"target uuid {target_uuid!r} not found in before records "
                    f"({len(before.records)} record(s))",
                    f"before chain uuids: {[record_uuid(r) for r in before_chain]}",
                ],
                sections=[before.describe()],
                session_id=after.session_id,
            )
        )
    if is_event(target):
        raise HistoryAssertionError(
            _failure(
                "rewind transition violated",
                [
                    f"target uuid {target_uuid!r} is an event record, not a Message "
                    f"({describe_record(target, line=before.line_of(target_uuid))})"
                ],
                sections=[before.describe()],
                session_id=after.session_id,
            )
        )

    problems: list[str] = []
    source: Mapping[str, Any] | None = None
    skipped_events: list[str] = []
    cursor = target.get("parent_uuid")
    while isinstance(cursor, str) and cursor:
        node = before.by_uuid.get(cursor)
        if node is None:
            raise HistoryAssertionError(
                _failure(
                    "rewind transition violated",
                    [
                        f"target {target_uuid!r}: parent_uuid={cursor!r} is not in "
                        "the before record set —— 链断裂，无法确定复制源",
                        describe_record(target, line=before.line_of(target_uuid)),
                    ],
                    sections=[before.describe()],
                    session_id=after.session_id,
                )
            )
        if is_message(node):
            source = node
            break
        skipped_events.append(cursor)
        parent = node.get("parent_uuid")
        cursor = parent if isinstance(parent, str) and parent else None

    if not after_chain:
        raise HistoryAssertionError(
            _failure(
                "rewind transition violated",
                ["after active chain is empty (rewind 必须落一条新 tip)"],
                sections=[after.describe()],
                session_id=after.session_id,
            )
        )

    # ── 新 tip + 前缀链形状 ──
    #
    # 事件节点容许跟随在复制行之后（事实上 rewind 路径目前不落任何事件），
    # 但复制行必须是链上的最后一条 **Message**。
    copy_index = None
    for index in range(len(after_chain) - 1, -1, -1):
        if is_message(after_chain[index]):
            copy_index = index
            break
    if copy_index is None:
        raise HistoryAssertionError(
            _failure(
                "rewind transition violated",
                ["after active chain contains no Message node"],
                sections=[describe_chain(after_chain, title="after chain")],
                session_id=after.session_id,
            )
        )
    copy = after_chain[copy_index]
    copy_uuid = record_uuid(copy)

    tail = after_chain[copy_index + 1 :]
    for offset, record in enumerate(tail):
        position = copy_index + 1 + offset
        if is_message(record) and not allow_trailing:
            problems.append(
                f"after chain[{position}] unexpected extra Message node "
                f"({describe_record(record, position=position)})——"
                "复制行之后不得出现计划外的 Message（若断言发生在后续演进之后，"
                "用 allow_trailing=True 明确放行）"
            )

    source_uuid = record_uuid(source) if source is not None else None
    if source is None:
        # 根形态：实现写入 [rewind_to_root] 哨兵
        if copy.get("role") != "system":
            problems.append(
                f"copy node role must be 'system' for a rewind-to-root, got "
                f"{copy.get('role')!r} ({describe_record(copy, position=copy_index)})"
            )
        if copy.get("content") != REWIND_TO_ROOT_CONTENT:
            problems.append(
                f"copy node content must be {REWIND_TO_ROOT_CONTENT!r} for a "
                f"rewind-to-root, got {truncate(str(copy.get('content')), 120)!r} "
                f"({describe_record(copy, position=copy_index)})"
            )
        grandparent = copy.get("parent_uuid")
        if grandparent:
            problems.append(
                f"rewind-to-root copy must not have a parent_uuid, got "
                f"{grandparent!r} ({describe_record(copy, position=copy_index)})"
            )
        expected_prefix: list[Mapping[str, Any]] = []
        mode = "root"
    else:
        diff = semantic_diff(
            node_semantics(source),
            node_semantics(copy),
        )
        for line in diff:
            problems.append(
                f"copy node ({describe_record(copy, position=copy_index)}) differs "
                f"from its source Message ({describe_record(source, line=before.line_of(source_uuid or ''))}): "
                f"{line}"
            )
        grandparent = source.get("parent_uuid")
        if copy.get("parent_uuid") != grandparent:
            problems.append(
                f"copy node parent_uuid={copy.get('parent_uuid')!r} != grandparent "
                f"{grandparent!r} (source Message uuid={source_uuid!r})——"
                "复制行的 parent 指向 target 的祖父"
            )
        expected_prefix = (
            before.chain_ending_at(grandparent)
            if isinstance(grandparent, str) and grandparent
            else []
        )
        mode = "copy"

    prefix_actual = after_chain[:copy_index]
    expected_prefix_uuids = [record_uuid(record) for record in expected_prefix]
    actual_prefix_uuids = [record_uuid(record) for record in prefix_actual]
    if expected_prefix_uuids != actual_prefix_uuids:
        problems.append(
            f"copy 之前的前缀链不一致：expected {expected_prefix_uuids} "
            f"got {actual_prefix_uuids} —— rewind 只追加复制行，不改写既有节点"
        )
    if copy_uuid is None or copy_uuid in before_uuids:
        problems.append(
            f"copy node reuses uuid {copy_uuid!r} from before records "
            f"({describe_record(copy, position=copy_index)})——复制行必须是全新 uuid"
        )

    # ── target 及其后续：不在活跃链但仍在记录集 ──
    dropped_uuids: list[str] = []
    after_chain_uuids = {record_uuid(record) for record in after_chain}
    if target_uuid in after_chain_uuids:
        problems.append(
            f"target uuid {target_uuid!r} is still on the after active chain —— "
            "rewind 之后 target 及其后续必须被移出链"
        )
    before_chain_uuids = [record_uuid(record) for record in before_chain]
    if target_uuid in before_chain_uuids:
        start = before_chain_uuids.index(target_uuid)
        for record in before_chain[start:]:
            uuid = record_uuid(record)
            if uuid is None:
                continue
            dropped_uuids.append(uuid)
            if uuid in after_chain_uuids:
                problems.append(
                    f"node {uuid!r} (at/after target {target_uuid!r} in the before "
                    "chain) is still on the after active chain"
                )
            if uuid not in after.by_uuid:
                problems.append(
                    f"node {uuid!r} (at/after target {target_uuid!r}) disappeared from "
                    "after.records —— rewind 只移动 tip，不删记录"
                )

    if problems:
        raise HistoryAssertionError(
            _failure(
                "rewind transition violated",
                problems,
                sections=[
                    describe_chain(before_chain, title="before chain"),
                    describe_chain(after_chain, title="after chain"),
                ],
                session_id=after.session_id,
            )
        )

    return {
        "mode": mode,
        "copy_uuid": copy_uuid,
        "source_uuid": source_uuid,
        "target_uuid": target_uuid,
        "expected_draft": (
            target.get("content") if isinstance(target.get("content"), str) else ""
        ),
        "dropped_uuids": dropped_uuids,
        "skipped_event_uuids": skipped_events,
    }


# ── 过渡断言 3：fork ──────────────────────────────────────


def fork_metadata(
    child_session_dir: Path | str,
    *,
    require: Sequence[str] = FORK_METADATA_FIELDS,
) -> dict[str, Any]:
    """独立解析 fork 子 session 的 ``metadata.json`` 并断言必需字段存在。

    Args:
        child_session_dir: 子 session 目录（``env.session_dir(child_id)``）。
        require: 必需字段（默认 :data:`FORK_METADATA_FIELDS`；传 ``()`` 退化为
            纯解析）。

    Returns:
        解析出的 metadata 字典。

    Raises:
        HistoryAssertionError: metadata.json 缺失 / 损坏 / 缺必需字段。
    """
    path = Path(child_session_dir) / "metadata.json"
    data = read_metadata(Path(child_session_dir))
    if data is None:
        raise HistoryAssertionError(
            f"fork child metadata.json missing at {path} —— fork 必须一次写全"
            f"元数据快照 {list(FORK_METADATA_FIELDS)}"
        )
    missing = [key for key in require if not data.get(key)]
    if missing:
        raise HistoryAssertionError(
            f"fork child metadata.json missing required field(s) {missing} at {path}:\n"
            f"  require: {list(require)}\n"
            f"  actual : {json.dumps(data, ensure_ascii=False, sort_keys=True)}"
        )
    return data


def assert_fork_of(
    source: HistoryView,
    child: HistoryView,
    at_uuid: str,
    *,
    check_metadata: bool = True,
    require_metadata: Sequence[str] = FORK_METADATA_FIELDS,
    snapshot_fields: Sequence[str] = FORK_SNAPSHOT_FIELDS,
    allow_trailing: bool = False,
) -> dict[str, Any]:
    """fork 的链完整性与 metadata 快照对账（spec「fork 断言」、tasks 5.3）。

    断言（``child`` 须是 fork 之后、子 session 后续演进之前的快照）：

    - 子链 == 源链中 ``at_uuid`` 之前的完整前缀（语义逐节点等价、不含
      ``at_uuid`` 自身）：期望值按实现的 ``walk_full_chain(from_uuid=at.parent)``
      口径取自源视图（含压缩节点、跨压缩边界）；
    - uuid 全量重映射：子链 uuid 与源记录集**无交集**，且映射保持 parent 关系；
    - 事件记录随行拷贝（**前缀窗口内**的事件类型序列 + 载荷逐项等价，
      剔除链拓扑与 ``ts``；子 session 后续自产事件不算"没随行"）；
    - ``metadata.json`` 快照：``fork_metadata`` 校验必需字段存在，
      ``forked_from`` 恒等于源 session id，``snapshot_fields`` 与源 metadata
      逐字段对账。

    **快照对账的边界**：``snapshot_fields`` 在源 session 上是**当前值**
    （模型 / 模板 / workspace 都可在 fork 之后被改），子 session 记录的是
    **fork 时刻**的快照——两者只在"断言紧跟 fork"时必然相等。若场景在源
    session 上切换过模型 / 模板 / 工作目录之后再断言，请传
    ``snapshot_fields=()``（此时仍校验存在性与 ``forked_from``）。
    源侧缺该字段记录时该字段自然跳过，并在返回值的
    ``unverifiable_metadata_fields`` 里如实列出。

    Args:
        at_uuid: fork 目标消息 uuid（或哨兵 ``"current"`` = 复制整链）。
        check_metadata: 关闭即放弃 metadata 快照守卫（仅用于纯链形状对账，
            场景**不应**使用）。
        require_metadata: 传给 :func:`fork_metadata` 的必需字段。
        snapshot_fields: 与源 metadata 交叉对账的字段
            （默认 :data:`FORK_SNAPSHOT_FIELDS`）。
        allow_trailing: 放行子链末尾的**计划外 Message**（默认严格）。

    Returns:
        对账素材字典（``expected_draft``（fork 的 ``draft`` 应等于它）、
        ``uuid_map``（源 uuid → 子 uuid）、``chain_length``、``trailing_uuids``、
        ``child_metadata``、``unverifiable_metadata_fields``）。

    Raises:
        HistoryAssertionError: 链形状或 metadata 断言失败。
    """
    source_chain = source.active_chain()
    source_uuids = set(source.by_uuid)

    # ── 期望前缀（镜像 extract_subchain 的 walk 口径） ──
    if at_uuid == CURRENT_SENTINEL:
        expected_draft: str | None = ""
        expected = source.full_chain()
    else:
        at = source.by_uuid.get(at_uuid)
        if at is None:
            raise HistoryAssertionError(
                _failure(
                    "fork assertion violated",
                    [
                        f"at uuid {at_uuid!r} not found in source records "
                        f"({len(source.records)} record(s))",
                        f"source chain uuids: {[record_uuid(r) for r in source_chain]}",
                    ],
                    sections=[source.describe()],
                    session_id=child.session_id,
                )
            )
        if is_event(at):
            raise HistoryAssertionError(
                _failure(
                    "fork assertion violated",
                    [
                        f"at uuid {at_uuid!r} is an event record, not a Message "
                        f"({describe_record(at, line=source.line_of(at_uuid))})"
                    ],
                    sections=[source.describe()],
                    session_id=child.session_id,
                )
            )
        expected_draft = at.get("content") if isinstance(at.get("content"), str) else ""
        parent = at.get("parent_uuid")
        expected = (
            source.full_chain(parent) if isinstance(parent, str) and parent else []
        )

    child_chain = child.active_chain()
    problems, trailing = _semantic_prefix_problems(
        expected,
        child_chain,
        expected_label="source prefix",
        actual_label="child chain",
        allow_trailing=allow_trailing,
    )

    # ── uuid 全量重映射 ──
    child_uuids = [record_uuid(record) for record in child.by_uuid.values()]
    overlap = sorted({uuid for uuid in child_uuids if uuid} & source_uuids)
    if overlap:
        problems.append(
            f"child uuids intersect the source record set ({len(overlap)}): "
            f"{overlap} —— fork 必须全量重映射 uuid"
        )
    uuid_map: dict[str, str] = {}
    for position, (want, got) in enumerate(zip(expected, child_chain)):
        want_uuid = record_uuid(want)
        got_uuid = record_uuid(got)
        if want_uuid is None or got_uuid is None:
            continue
        if want_uuid in uuid_map:
            problems.append(
                f"source uuid {want_uuid!r} appears twice in the expected prefix "
                "(fork 前源链已损坏)"
            )
        uuid_map[want_uuid] = got_uuid
    mapped_values = [uuid for uuid in uuid_map.values()]
    if len(set(mapped_values)) != len(mapped_values):
        problems.append(
            f"child chain reuses uuids for distinct source nodes: {uuid_map} —— "
            "重映射必须是单射"
        )

    # ── parent 关系保持 + 子链闭合 ──
    for position in range(1, len(child_chain)):
        parent = child_chain[position].get("parent_uuid")
        expected_parent = record_uuid(child_chain[position - 1])
        if parent != expected_parent:
            problems.append(
                f"child chain[{position}] "
                f"({describe_record(child_chain[position], position=position)}): "
                f"parent_uuid={parent!r} != child chain[{position - 1}].uuid="
                f"{expected_parent!r} —— parent 关系必须保持"
            )
    if child_chain and child_chain[0].get("parent_uuid"):
        problems.append(
            f"child chain root ({describe_record(child_chain[0], position=0)}): "
            f"parent_uuid={child_chain[0].get('parent_uuid')!r} —— "
            "前缀根节点没有 parent（重映射后应为空）"
        )

    # ── 事件随行（只对前缀窗口内的事件） ──
    #
    # 注意窗口：子 session 在 fork 之后会继续演进、产生自己的事件——那不是
    # "事件没随行"。事件随行只对 fork 前缀（前 len(expected) 个节点）成立。
    expected_events = [record for record in expected if is_event(record)]
    child_events = [
        record for record in child_chain[: len(expected)] if is_event(record)
    ]
    if [record.get("type") for record in expected_events] != [
        record.get("type") for record in child_events
    ]:
        problems.append(
            f"event records did not follow the fork (前 {len(expected)} 个节点窗口): "
            f"expected {len(expected_events)} event node(s) "
            f"({[record.get('type') for record in expected_events]}), got "
            f"{len(child_events)} ({[record.get('type') for record in child_events]})"
        )

    if problems:
        raise HistoryAssertionError(
            _failure(
                "fork assertion violated",
                problems,
                sections=[
                    describe_chain(expected, title="source prefix (expected)"),
                    describe_chain(child_chain, title="child chain"),
                ],
                session_id=child.session_id,
            )
        )

    # ── metadata 快照 ──
    child_metadata: dict[str, Any] | None = None
    unverifiable: list[str] = []
    if check_metadata:
        child_metadata = fork_metadata(child.session_dir, require=require_metadata)
        source_metadata = source.metadata()
        if source_metadata is None:
            raise HistoryAssertionError(
                _failure(
                    "fork assertion violated",
                    [
                        f"source metadata.json missing at {source.metadata_path}"
                        " —— 无法对账 fork 的 metadata 快照"
                    ],
                    sections=[source.describe()],
                    session_id=child.session_id,
                )
            )
        metadata_problems: list[str] = []
        forked_from = child_metadata.get("forked_from")
        if forked_from != source.session_id:
            metadata_problems.append(
                f"metadata forked_from: expected {source.session_id!r} "
                f"got {forked_from!r}"
            )
        for key in snapshot_fields:
            if key not in source_metadata:
                # 源 session 自己没有该记录（如从未切换过模型）→ 无法对账，
                # 仅校验子侧存在性（fork_metadata 已完成）。
                unverifiable.append(key)
                continue
            if child_metadata.get(key) != source_metadata[key]:
                metadata_problems.append(
                    f"metadata {key}: expected {source_metadata[key]!r} "
                    f"(fork 时刻快照 = 源当前值) got {child_metadata.get(key)!r}"
                )
        if metadata_problems:
            raise HistoryAssertionError(
                _failure(
                    "fork metadata snapshot violated",
                    metadata_problems,
                    sections=[
                        f"child metadata: "
                        f"{json.dumps(child_metadata, ensure_ascii=False, sort_keys=True)}",
                        f"source metadata: "
                        f"{json.dumps(source_metadata, ensure_ascii=False, sort_keys=True)}",
                        (
                            f"无法与源对账的字段（源侧无记录）: {unverifiable}"
                            if unverifiable
                            else ""
                        ),
                    ],
                    session_id=child.session_id,
                )
            )

    return {
        "expected_draft": expected_draft,
        "uuid_map": uuid_map,
        "prefix_length": len(expected),
        "chain_length": len(child_chain),
        "trailing_uuids": [record_uuid(record) for record in trailing],
        "child_metadata": child_metadata,
        "unverifiable_metadata_fields": unverifiable,
    }


__all__ = [
    "CURRENT_SENTINEL",
    "DELTA_EVENT_TYPES",
    "FORK_METADATA_FIELDS",
    "FORK_SNAPSHOT_FIELDS",
    "TRANSIENT_EVENT_TYPES",
    "assert_chain_invariants",
    "assert_compact_transition",
    "assert_fork_of",
    "assert_no_transient_records",
    "assert_rewind_transition",
    "assert_tool_pairing",
    "fork_metadata",
]
