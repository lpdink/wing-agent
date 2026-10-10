# wing/gateway/protocol/session.py — /api/session/* 协议模型

"""Session 端点协议模型——``/api/session/*`` 的请求与响应。

生命周期（create / resume / fork）、订阅（subscribe / unsubscribe）、消息发送
（send）、查询（list / get / info / branches）与状态变更（update / compact /
interrupt / release / rewind）。字段与文案即线格式：改动等于改协议。
"""

from __future__ import annotations

from pydantic import BaseModel, Field

from wing.event import AgentInfo, SessionInfo
from wing.event.query_response import BranchTargetInfo
from wing.session.override import AgentOverride

# 与 `wing.event.SessionInfo.tag_meta` 同一记录类型（线格式 = 持久记录形态，
# 刻意不另立一份：见 `wing/event/base.py` 的同一说明）。
from wing.store import TagMeta


# ============================================================
# 请求模型
# ============================================================


class CreateSessionRequest(BaseModel):
    """创建新 session 的请求体。"""

    template_name: str | None = Field(
        default=None, description="Agent 模板名称，None 使用默认模板"
    )
    workspace: str | None = Field(default=None, description="工作目录路径")
    agent: AgentOverride | None = Field(default=None, description="Agent 参数覆盖")
    backend: str | None = Field(
        default=None,
        description=(
            "存储后端：file（默认，落盘）| memory（session 状态不落盘，仅本次进程有效；"
            "注意 metrics 审计文件不受此约束）"
        ),
    )
    tags: list[str] | None = Field(
        default=None,
        description="创建即打标（校验语义同 /api/session/tag；非法 400）",
    )
    session_id: str | None = Field(
        default=None,
        description=(
            "指定 session id（**create-or-adopt**）：不存在则以该 id 建会话；"
            "已存在（内存或任一 store）则收养既有会话（语义同 /api/session/resume，"
            "agent 覆盖只应用 resume 子集）。缺省 = 后端自生成。"
            "**收养路径忽略 template_name / workspace / backend**——它们以 metadata "
            "为准且不做校验（同一个请求体因此可能'id 存在 → 200 / id 不存在 → 400'，"
            "例如 backend 非法时）。"
            "id 只做基本卫生校验（非空、≤128 字节、可编码为 UTF-8、无路径分隔符 / "
            "'..' / 控制字符、不以 '.' 开头），不合规 400 且不产生任何残留。"
            "同一文件系统内一个 id 只对应一个会话：大小写 / 归一化不敏感的 FS 上"
            "别名会被归一到磁盘真名并在响应里回报（日志留 warning）。"
        ),
    )


class ResumeSessionRequest(BaseModel):
    """恢复已有 session 的请求体。"""

    session_id: str = Field(description="要恢复的 session ID")
    agent: AgentOverride | None = Field(
        default=None,
        description=(
            "恢复时应用的参数覆盖——只应用 model_id / effort / tools 子集"
            "（system_prompt / append_system_prompt / max_turns / yolo 一律不应用："
            "它们会改变对话前缀或会话既有限额，是创建期语义）。被忽略的字段会记 "
            "warning。"
        ),
    )


class ForkSessionRequest(BaseModel):
    """分叉 session 的请求体。"""

    source_session_id: str = Field(description="源 session ID")
    target_uuid: str = Field(description="分叉点消息的 UUID")


class SubscribeRequest(BaseModel):
    """订阅 session 事件的请求体。需要 X-Client-Id header。"""

    session_id: str = Field(description="要订阅的 session ID")


class UnsubscribeRequest(BaseModel):
    """取消订阅 session 事件的请求体。需要 X-Client-Id header。"""

    session_id: str = Field(description="要取消订阅的 session ID")


class SendMessageRequest(BaseModel):
    """向 session 发送消息的请求体。"""

    session_id: str = Field(description="目标 session ID")
    content: str = Field(description="消息内容")
    tool_call_id: str | None = Field(
        default=None,
        description="回复某个 Ask 事件时携带其 tool_call_id，定向 resolve feedback waiter",
    )


class CompactRequest(BaseModel):
    """压缩 session 上下文的请求体。"""

    session_id: str = Field(description="目标 session ID")
    instruction: str | None = Field(
        default=None,
        description="用户下发的压缩侧重指令（如“保留架构决策与未完成的 TODO”），"
        "附加到压缩 prompt；缺省使用默认压缩策略",
    )


class InterruptRequest(BaseModel):
    """中断 session 当前任务的请求体。"""

    session_id: str = Field(description="目标 session ID")


class ReleaseRequest(BaseModel):
    """逐出（release）session 内存态的请求体。"""

    session_id: str = Field(description="目标 session ID")


class RewindRequest(BaseModel):
    """回退 session 到指定消息节点的请求体。"""

    session_id: str = Field(description="目标 session ID")
    target_uuid: str = Field(description="要回退到的消息 UUID")


class UpdateSessionRequest(BaseModel):
    """POST /api/session/update 请求——统一 session 状态变更。"""

    session_id: str = Field(description="目标 session ID")
    model_id: str | None = Field(
        default=None,
        description="切换模型（引用 providers[].models 的 id；未命中 400）",
    )
    agent: str | None = Field(default=None, description="切换 agent 模板")
    title: str | None = Field(default=None, description="设置 session 名称")
    thinking: bool | None = Field(default=None, description="开关 thinking 模式")
    reasoning_effort: str | None = Field(
        default=None, description="推理力度: low|medium|high|xhigh|max"
    )
    yolo: bool | None = Field(default=None, description="开关 yolo 模式")
    workspace: str | None = Field(default=None, description="切换工作目录路径")
    tools: list[str] | None = Field(
        default=None,
        description="切换工具集（全量替换，ref 格式：namespace.name 或裸名）",
    )


class TagSessionRequest(BaseModel):
    """POST /api/session/tag 请求——读取或原子增删会话标签。

    标签是不透明字符串（建议全小写，``k=v`` 作命名空间约定）；单个长度
    1..64，禁空白 / 控制字符 / 逗号，不以 ``-`` 开头，单会话上限 64 个。
    ``add`` / ``remove`` 皆缺省（None）= 纯读取，不产生任何变更。
    """

    session_id: str = Field(description="目标 session ID")
    add: list[str] | None = Field(
        default=None,
        description="要添加的标签（幂等；已存在为 no-op）",
    )
    remove: list[str] | None = Field(
        default=None,
        description="要移除的标签（幂等；不存在为 no-op；与 add 同值则移除胜出）",
    )


# ============================================================
# 响应模型
# ============================================================


class ReleaseResponse(BaseModel):
    """逐出（release）响应。

    released=False（detail="not loaded"）表示会话本就不在内存——幂等，
    不视为错误：它已经在「逐出」这个目标状态里了。被钉住（忙碌 / 有待处理
    输入 / 被订阅 / 非持久后端）时以 409 拒绝。
    """

    ok: bool = Field(default=True, description="操作是否成功")
    released: bool = Field(description="本次调用是否真的把会话逐出了内存")
    detail: str = Field(description="结果说明（released / not loaded）")


class CreateSessionResponse(BaseModel):
    """创建 session 的响应。"""

    session_id: str = Field(description="新创建的 session ID")
    template_name: str = Field(description="使用的模板名称")
    workspace: str | None = Field(default=None, description="工作目录路径")
    backend: str = Field(default="file", description="存储后端（file/memory）")


class ResumeSessionResponse(BaseModel):
    """恢复 session 的响应。"""

    session_id: str = Field(description="恢复的 session ID")
    template_name: str | None = Field(default=None, description="使用的模板名称")
    workspace: str | None = Field(default=None, description="工作目录路径")


class ForkSessionResponse(BaseModel):
    """分叉 session 的响应。"""

    session_id: str = Field(description="新分叉出的 session ID")
    draft: str | None = Field(
        default=None, description="分叉点处的 draft 消息（如果有）"
    )


class OkResponse(BaseModel):
    """通用成功响应。"""

    ok: bool = Field(default=True, description="操作是否成功")


class SendMessageResponse(BaseModel):
    """发送消息的响应。"""

    ok: bool = Field(default=True, description="操作是否成功")
    request_id: str = Field(description="请求 ID，用于前端关联 agent 响应")


class SessionListResponse(BaseModel):
    """session 列表响应。"""

    sessions: list[SessionInfo] = Field(description="所有活跃 session 的摘要列表")


class SessionGetResponse(BaseModel):
    """session 详情响应。"""

    session_id: str = Field(description="session ID")
    name: str | None = Field(default=None, description="session 名称")
    template_name: str | None = Field(default=None, description="使用的模板名称")
    workspace: str | None = Field(default=None, description="工作目录路径")
    status: str = Field(
        default="idle", description="运行时状态: inactive|idle|working|waiting"
    )
    messages: list[dict] = Field(description="消息历史列表")
    agent: AgentInfo | None = Field(default=None, description="当前 agent 配置信息")


class ContextStatsInfo(BaseModel):
    """上下文统计信息，嵌入 SessionInfoResponse。"""

    message_count: int = Field(description="当前消息数量")
    total_tokens: int = Field(description="当前上下文 token 总数")


class SessionInfoResponse(BaseModel):
    """GET /api/session/info 响应——session 运行时状态。"""

    model: str = Field(description="当前模型名称（实际调用名）")
    model_id: str | None = Field(
        default=None,
        description=(
            "当前模型的引用词（∈ providers[].models 的 id；不可用时 None）。"
            "前端的选择态 / 文案匹配以它为准。"
        ),
    )
    provider_name: str | None = Field(
        default=None,
        description="当前模型的 provider 名（运行期事实；未取到时 None）",
    )
    model_display_name: str | None = Field(
        default=None,
        description="当前模型的展示名（未声明 / 空串 = None，前端回落 model）",
    )
    api_url: str = Field(description="API 基础 URL")
    tools: list[str] = Field(description="已启用的工具名称列表")
    total_tokens: int = Field(description="当前上下文 token 总数")
    context_window_tokens: int = Field(description="上下文窗口大小")
    thinking: bool = Field(description="thinking 模式是否开启")
    reasoning_effort: str | None = Field(
        default=None, description="推理力度: low|medium|high|xhigh|max"
    )
    yolo: bool = Field(description="yolo 模式是否开启")
    session_name: str | None = Field(default=None, description="session 名称")
    workdir: str | None = Field(
        default=None,
        description="session 工作目录（session workspace，非进程启动目录）",
    )
    status: str = Field(
        default="idle", description="运行时状态: inactive|idle|working|waiting"
    )
    context_stats: ContextStatsInfo = Field(description="上下文统计信息")
    skills_info: str = Field(default="", description="已安装的 skills 信息")
    system_prompt: str = Field(default="", description="完整系统提示词")
    tags: list[str] = Field(
        default_factory=list, description="会话标签（插入序；无标签为空列表）"
    )
    tag_meta: dict[str, TagMeta] = Field(
        default_factory=dict,
        description="每个标签的记录（键集 ⊆ tags）；当前含 added_at 打标时间",
    )


class CompactResponse(BaseModel):
    """POST /api/session/compact 响应。"""

    ok: bool = Field(default=True, description="操作是否成功")
    original_tokens: int = Field(default=0, description="压缩前 token 数")
    compressed_tokens: int = Field(default=0, description="压缩后 token 数")


class RewindResponse(BaseModel):
    """POST /api/session/rewind 响应。"""

    ok: bool = Field(default=True, description="操作是否成功")
    draft: str | None = Field(default=None, description="回退点处的用户消息草稿")


class BranchesResponse(BaseModel):
    """GET /api/session/branches 响应——可回退/分叉的消息节点列表。"""

    targets: list[BranchTargetInfo] = Field(
        default_factory=list, description="可回退/分叉的消息节点"
    )


class UpdateSessionResponse(BaseModel):
    """POST /api/session/update 响应。"""

    ok: bool = Field(default=True, description="操作是否成功")


class TagSessionResponse(BaseModel):
    """POST /api/session/tag 响应——变更后的标签全貌 + 实际增删。"""

    ok: bool = Field(default=True, description="操作是否成功")
    session_id: str = Field(description="目标 session ID")
    tags: list[str] = Field(
        default_factory=list,
        description="变更后的全量标签（插入序；已有保持原序、新增追加在后）",
    )
    added: list[str] = Field(
        default_factory=list, description="本次实际新增（幂等 no-op 不计）"
    )
    removed: list[str] = Field(
        default_factory=list, description="本次实际移除（幂等 no-op 不计）"
    )
    tag_meta: dict[str, TagMeta] = Field(
        default_factory=dict,
        description="变更后的全量标签记录（键集 ⊆ tags）；当前含 added_at 打标时间",
    )
