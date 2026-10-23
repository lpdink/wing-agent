# wing/gateway/protocol/usage.py — `GET /api/usage` 的请求 / 响应模型
"""用量聚合端点的线格式：``{range, group_by, totals, rows, truncated}``。

字段与文案即线格式（改动等于改协议）；Rust 侧 ``crates/wing-api-client/src/models.rs``
是手写镜像，改字段时两边一起改。

两条刻意不给的东西，消费方必须自己算：

- **派生值**：命中率 = ``cached_tokens / prompt_tokens``、工时 = ``llm_time_ms +
  tool_time_ms``——全仓一致的口径是「线格式不含派生值」；
- **标题**：``session`` 维度的 ``label`` 恒为 ``null``（本端点不做文件 I/O 去取名，
  CLI 自己 join ``/api/session/list``）。

``Measures`` 的 11 个整数**永远全在**（全零填充、永不省略）。某个维度上不可归因的
度量也填 0（例如 ``group_by=model`` 的 ``tool_calls``——工具行没有 model 列）：
可归因性表见 ``docs/dev/usage.md``，``totals`` 永远由独立的全量查询给出。
"""

from __future__ import annotations

from pydantic import BaseModel, Field


class UsageRange(BaseModel):
    """统计窗口（闭区间，``YYYY-MM-DD``；「一天」= 网关本地时区的 00:00–24:00）。"""

    since: str = Field(description="窗口起点（含）；`since=all` 时 = 有记录的第一天")
    until: str = Field(description="窗口终点（含）")
    data_since: str | None = Field(
        default=None,
        description=(
            "审计流水（三张 `audit_*`）最早的一天；没有任何审计行 = null。"
            "早于它的日子只有会话计数（`session_catalog` 的回填覆盖得更早），"
            "消费方据此标注「token / 调用统计从这里起」"
        ),
    )


class UsageMeasures(BaseModel):
    """11 个计数（全零填充、永不省略、无派生值）。"""

    sessions: int = Field(
        default=0,
        description="桶内有 ≥1 条审计事实（LLM 调用 / 工具调用 / 用户消息）的不同会话数",
    )
    sessions_created: int = Field(
        default=0, description="桶内创建的会话数（含回填的历史会话）"
    )
    user_messages: int = Field(
        default=0, description="`user_message_accepted` 事件数（被接受的用户输入）"
    )
    llm_calls: int = Field(default=0, description="LLM 调用次数")
    tool_calls: int = Field(default=0, description="真实执行的工具调用次数")
    tool_errors: int = Field(
        default=0, description="工具抛错 / 超时的次数（Bash 非零退出码也算）"
    )
    prompt_tokens: int = Field(default=0, description="输入 token（含缓存命中部分）")
    completion_tokens: int = Field(default=0, description="输出 token")
    cached_tokens: int = Field(default=0, description="输入里命中缓存的部分")
    llm_time_ms: int = Field(default=0, description="LLM 调用时长合计（毫秒）")
    tool_time_ms: int = Field(default=0, description="工具执行时长合计（毫秒）")


class UsageRow(UsageMeasures):
    """一个桶（维度的一行）：键 + 可选展示素材 + 11 个度量（扁平）。"""

    key: str = Field(
        description="桶的键：日期 / `provider/model` / session id / 工具名 / "
        "workspace / 模板名（空值 = 空串）/ `me`|`agents`"
    )
    label: str | None = Field(
        default=None, description="展示名（仅 `group_by=model` 会填；解析不出为 null）"
    )
    parent_session_id: str | None = Field(
        default=None,
        description="血缘（仅 `group_by=session` 会填；供前端画树，根会话为 null）",
    )


class UsageResponse(BaseModel):
    """`GET /api/usage` 的响应体。"""

    range: UsageRange = Field(description="统计窗口")
    group_by: str = Field(description="本次的维度（回显请求值）")
    totals: UsageMeasures = Field(
        description="整个窗口的总量（独立查询，不随 `limit` 截断；对账基准）"
    )
    rows: list[UsageRow] = Field(
        description="按维度的行（`day` 稠密；`origin` 恰两行）"
    )
    truncated: bool = Field(
        default=False,
        description="行被 `limit` 截断了吗（只对 model / session / tool / "
        "workspace / agent 有意义）",
    )
