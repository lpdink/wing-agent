# wing/metrics/measures.py — 用量聚合的领域值类型（无 I/O、无 SQL）
"""聚合结果的两个值类型：:class:`Measures`（一行度量）与 :class:`Row`（一个桶）。

``Measures`` 是**唯一的度量词表**：11 个整数、缺省 0、永不省略。“缺省 0”不是
“测量得 0”的同义词——某一维度上不可归因的度量也填 0（例如工具行没有 model 列，
所以 ``group_by=model`` 的 ``tool_calls`` 恒为 0）。可归因性表见
``docs/dev/usage.md``；``totals`` 永远由独立的全量查询算出，是唯一的对账基准。

不做派生值：命中率 = ``cached_tokens / prompt_tokens``、工时 = ``llm_time_ms +
tool_time_ms`` 一律由消费方算（与全仓既有口径一致——线格式不放派生值）。
"""

from __future__ import annotations

from dataclasses import dataclass

#: 度量的字段名（顺序即 wire 上 ``Measures`` 的声明顺序；测试据此对账协议模型）。
MEASURE_FIELDS: tuple[str, ...] = (
    "sessions",
    "sessions_created",
    "user_messages",
    "llm_calls",
    "tool_calls",
    "tool_errors",
    "prompt_tokens",
    "completion_tokens",
    "cached_tokens",
    "llm_time_ms",
    "tool_time_ms",
)


@dataclass(frozen=True, slots=True)
class Measures:
    """一个桶（天 / 会话 / 模型 / 工具 / 工作区 / 模板 / 归属）上的 11 个计数。"""

    sessions: int = 0
    """桶内有 ≥1 条审计事实（LLM 调用 / 工具调用 / 用户消息）的不同会话数。"""

    sessions_created: int = 0
    """桶内创建的会话数（``session_catalog.created_at`` 落在桶内，含回填的历史会话）。"""

    user_messages: int = 0
    """``user_message_accepted`` 事件数（被接受的用户输入——含编排方投递与唤醒信封）。"""

    llm_calls: int = 0
    tool_calls: int = 0
    tool_errors: int = 0
    """工具抛错 / 超时的次数。``Bash`` 非零退出码也算（success = 退出码 0）。"""

    prompt_tokens: int = 0
    """含缓存命中部分的输入 token（``cached_tokens`` 是它的子集）。"""

    completion_tokens: int = 0
    cached_tokens: int = 0
    llm_time_ms: int = 0
    tool_time_ms: int = 0

    def __add__(self, other: Measures) -> Measures:
        return Measures(*[a + b for a, b in zip(self.astuple(), other.astuple())])

    def astuple(self) -> tuple[int, ...]:
        """按 :data:`MEASURE_FIELDS` 顺序展开成整数元组（求和 / 投影用）。"""
        return tuple(getattr(self, name) for name in MEASURE_FIELDS)

    def wire(self) -> dict[str, int]:
        """wire 形状：11 个键全在（``response_model`` 逐字段校验的就是这份）。"""
        return dict(zip(MEASURE_FIELDS, self.astuple()))


def total(measures: list[Measures]) -> Measures:
    """一组度量的和（空列表 → 全零）。"""
    out = Measures()
    for item in measures:
        out = out + item
    return out


@dataclass(frozen=True, slots=True)
class Row:
    """聚合结果的一行：桶的键 + 可选展示素材 + 该桶的 11 个度量。

    ``key`` 的语义随维度变（日期 / ``provider/model`` / session id / 工具名 /
    workspace 绝对路径 / 模板名 / ``me``|``agents``）；``label`` 只有 ``model``
    维度会填（配置里的展示名），``parent_session_id`` 只有 ``session`` 维度会填
    （供前端画血缘树）。
    """

    key: str
    measures: Measures
    label: str | None = None
    parent_session_id: str | None = None

    def wire(self) -> dict[str, object]:
        """wire 形状：``{key, label, parent_session_id, ...11 个度量}``（扁平）。"""
        return {
            "key": self.key,
            "label": self.label,
            "parent_session_id": self.parent_session_id,
            **self.measures.wire(),
        }
