"""history —— ``history.jsonl`` 独立解析与上下文红线断言（tasks 5.1–5.3）。

模块划分：

- :mod:`wing_probe.history.view`：``HistoryView``（记录 / 索引 / 活跃链 /
  ``messages()`` / ``events()`` / ``full_chain()``）、语义投影与差异工具、
  ``metadata.json`` 解析；
- :mod:`wing_probe.history.invariants`：三条内置不变量
  （链拓扑 / tool 配对 / 瞬态不落盘）、三条红线过渡断言
  （compact / rewind / fork）、``fork_metadata``。

典型用法（场景内）：

    from wing_probe.history import HistoryView, assert_rewind_transition

    before = HistoryView(env.session_dir(session.session_id))
    result = await session.rewind(target_uuid)
    after = HistoryView(env.session_dir(session.session_id))
    material = assert_rewind_transition(before, after, target_uuid)
    assert result["draft"] == material["expected_draft"]
"""

from wing_probe.history.invariants import (
    CURRENT_SENTINEL,
    DELTA_EVENT_TYPES,
    FORK_METADATA_FIELDS,
    FORK_SNAPSHOT_FIELDS,
    TRANSIENT_EVENT_TYPES,
    assert_chain_invariants,
    assert_compact_transition,
    assert_fork_of,
    assert_no_transient_records,
    assert_rewind_transition,
    assert_tool_pairing,
    fork_metadata,
)
from wing_probe.history.view import (
    COMPACT_PREFIX,
    EVENT_ROLE,
    HISTORY_FILE,
    METADATA_FILE,
    MESSAGE_ROLES,
    REWIND_TO_ROOT_CONTENT,
    TOPOLOGY_KEYS,
    HistoryAssertionError,
    HistoryView,
    describe_chain,
    describe_record,
    event_semantics,
    is_event,
    is_message,
    message_semantics,
    node_semantics,
    read_metadata,
    record_uuid,
    semantic_diff,
    truncate,
)

__all__ = [
    "COMPACT_PREFIX",
    "CURRENT_SENTINEL",
    "DELTA_EVENT_TYPES",
    "EVENT_ROLE",
    "FORK_METADATA_FIELDS",
    "FORK_SNAPSHOT_FIELDS",
    "HISTORY_FILE",
    "METADATA_FILE",
    "MESSAGE_ROLES",
    "REWIND_TO_ROOT_CONTENT",
    "TOPOLOGY_KEYS",
    "TRANSIENT_EVENT_TYPES",
    "HistoryAssertionError",
    "HistoryView",
    "assert_chain_invariants",
    "assert_compact_transition",
    "assert_fork_of",
    "assert_no_transient_records",
    "assert_rewind_transition",
    "assert_tool_pairing",
    "describe_chain",
    "describe_record",
    "event_semantics",
    "fork_metadata",
    "is_event",
    "is_message",
    "message_semantics",
    "node_semantics",
    "read_metadata",
    "record_uuid",
    "semantic_diff",
    "truncate",
]
