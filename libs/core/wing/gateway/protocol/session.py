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
from wing.schema import MediaRef
from wing.session.override import AgentOverride

# 与 `wing.event.SessionInfo.tag_meta` 同一记录类型（线格式 = 持久记录形态，
# 刻意不另立一份：见 `wing/event/base.py` 的同一说明）。ArchivedSession 同理：
# 归档条目就是存储层模型（含投影期补全的展示名），不另立一份镜像。
from wing.store import ArchivedSession, FileRef, SessionUsage, TagMeta


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
            "注意审计行（wing.db）不受此约束）"
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
    parent_session_id: str | None = Field(
        default=None,
        description=(
            "创建者会话 id（**血缘**：谁创建了我；由 CLI 从 WING_SESSION_ID 填入，"
            "显式调用方可给任意格式合法的 id）。创建时一次写入 metadata，此后没有"
            "修改路径；缺省 = 根会话（零字段）。只做格式卫生（同 session_id 规则）、"
            "**不校验父是否存在**（弱引用）；与 session_id 同时给出且字面相等"
            "（自指）→ 400。**收养路径忽略它**（resume 语义：既有会话的记录不被改写），"
            "日志留 warning。"
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
    media: list[MediaRef] | None = Field(
        default=None,
        description=(
            "消息携带的媒体引用（缺省 / null = 无媒体）。id 必须指向该会话已存在的"
            "媒体对象且 mime 在支持列表内，否则 400（整条消息被拒，不进历史）"
        ),
    )
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


class ArchiveRequest(BaseModel):
    """POST /api/session/archive 请求——设置会话的归档状态。

    ``archived`` 缺省 true（省略即归档）。归档 = 移入存储根的 ``.archive/``
    （纯移动，字节不变）；恢复 = 移回 live 区。归档区不参与列表与解析
    （冻结语义），钉住条件（忙碌 / 排队输入 / 订阅中）命中时 409。
    """

    session_id: str = Field(description="目标 session ID")
    archived: bool = Field(
        default=True, description="true = 归档（默认）；false = 恢复"
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
    parent_session_id: str | None = Field(
        default=None,
        description=(
            "**读回的**父会话 id（血缘——不是请求值的回声，同 tags 的读回惯例）："
            "None = 该会话无父（根会话 / 旧网关不带此字段），调用方据此对账请求是否"
            "被记录。收养路径请求里的 parent 被忽略（resume 语义），这里反映的是"
            "**既有记录**。"
        ),
    )


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


class MediaUploadResponse(BaseModel):
    """POST /api/session/media 响应——入库的媒体引用。

    ``media`` 与消息 ``media`` 数组的元素**同一形状**（``MediaRef``）：客户端可以把它
    原样放进 `/api/session/send` / WS `ClientRequest.media`，无需字段搬运。``name``
    缺省（未给 / 空串）时为 null。
    """

    ok: bool = Field(default=True, description="操作是否成功")
    media: MediaRef = Field(
        description="入库的媒体引用（id / mime / bytes / width / height / name）"
    )


class FileUploadResponse(BaseModel):
    """POST /api/session/files 响应——入库的文件引用（存储层 ``FileRef`` 的线格式）。

    ``file.name`` 是**实际落盘名**（同名异字节冲突时带 ``-2`` / ``-3`` 后缀，不是
    请求里的名字）；``file.path`` 是网关宿主机上的**绝对路径**，消费方把它写进消息
    正文（``[file: 名 · 大小 · 路径]``），agent 用 Read / Bash 直接读——memory
    后端没有本地文件，恒为 null（如实降级，不编路径）。
    """

    ok: bool = Field(default=True, description="操作是否成功")
    file: FileRef = Field(
        description="入库的文件（name = 实际落盘名；path = 绝对路径；bytes = 字节数）"
    )


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
    parent_session_id: str | None = Field(
        default=None,
        description=(
            "创建者会话 id（血缘：谁创建了我，见 docs/dev/session-lineage.md）；"
            "根会话为 None。弱引用：不保证父会话当前可解析。"
        ),
    )
    last_turn_end: str | None = Field(
        default=None,
        description=(
            "最近一次收口轮次的结束时刻（本地 naive ISO，与 last_interaction "
            "同口径）；还没有收口过（含中断收场——中断不发 turn_result）为 None"
        ),
    )
    last_turn_subtype: str | None = Field(
        default=None,
        description=(
            "该轮 turn_result 的 subtype: success|error_during_execution|"
            "error_max_turns（逐字透传；与 last_turn_end 同生同灭）"
        ),
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
    usage: SessionUsage = Field(
        default_factory=SessionUsage,
        description=(
            "会话用量与运行时长累计（计数 + token + 时长；跨 release / 网关"
            "重启延续）。全零 = 还没有可用量记录（旧会话从零开始，不回填）。"
            "命中率 = cached_tokens / prompt_tokens，工作时长 = "
            "llm_time_ms + tool_time_ms 由客户端推导。"
        ),
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


# ============================================================
# 缓存账本（GET /api/session/cache）——见 contract §6
# ============================================================


class CacheSummary(BaseModel):
    """账本汇总：三个 rate 的分母都是 Σ prompt；分母为 0 时全为 null。"""

    calls: int = Field(default=0, description="账本里的调用数（环形上界 200）")
    prompt_tokens: int = Field(default=0, description="Σ prompt_tokens")
    cached_tokens: int = Field(
        default=0, description="Σ cached_tokens（provider 报告）"
    )
    expected_tokens: int = Field(
        default=0, description="Σ expected（只累计有参照的调用）"
    )
    theory_rate: float | None = Field(
        default=None, description="理论命中率 = Σexpected / Σprompt；分母 0 → null"
    )
    actual_rate: float | None = Field(
        default=None, description="实际命中率 = Σcached / Σprompt；分母 0 → null"
    )
    provider_debt_rate: float | None = Field(
        default=None,
        description=(
            "provider 欠账率 = Σmax(0, expected-cached)（仅 verdict ∈ "
            "{provider_underdelivered, block_granularity}）/ Σprompt；分母 0 → null"
        ),
    )


class CacheForkDetail(BaseModel):
    """分叉点的证据：来源文件事实 + 对照侧时刻 + 两侧 sha。

    文件事实取自分叉处段落的 `src`（新侧优先，缺则对照侧）；非文件段落
    （`tools:` / `history:` / `system:*`）的 path/mtime/size 为 null。
    """

    path: str | None = Field(
        default=None, description="来源文件路径（contract §1 的 src.path）"
    )
    mtime: str | None = Field(
        default=None, description="来源文件 mtime（本地 naive ISO）"
    )
    size: int | None = Field(default=None, description="来源文件字节数")
    ref_ts: str | None = Field(
        default=None,
        description="对照侧调用的时刻（与 sha_before 同源；对照侧已滚出账本 → null）",
    )
    sha_before: str | None = Field(
        default=None, description="对照侧（记录时分叉相对的那条调用）该段落的 sha"
    )
    sha_after: str | None = Field(default=None, description="本次该段落的 sha")


class CacheForkRow(BaseModel):
    """一条分叉（只含 `fork != null` 的调用，新→旧）。"""

    ts: str = Field(description="调用时刻（本地 naive ISO，毫秒）")
    index: int = Field(description="分叉点 = 与最近一条调用的最长公共前缀长度")
    label: str = Field(description="分叉处的段 label（如 `skills:pdf-tools`）")
    kind: str = Field(description="insert | remove | modify")
    blame: str | None = Field(
        default=None,
        description="external | tools | composition | compact | provider | cold（派生，不落盘）",
    )
    avoidable: bool | None = Field(
        default=None,
        description="这次分叉是否可避免（None = 不适用，如 provider 欠兑现）",
    )
    tokens_lost: int | None = Field(
        default=None, description="≈ 丢失的 prompt token（估算；无参照 → null）"
    )
    segments_affected: int = Field(
        description="最近一条调用（分叉的对照侧）fork 点之后的段数（contract §4 的 len(C) - k）"
    )
    verdict: str = Field(description="该次调用的缓存判定（contract §4）")
    detail: CacheForkDetail = Field(
        default_factory=CacheForkDetail, description="分叉点证据"
    )


class CacheCallRow(BaseModel):
    """一次调用的账本行（最近 limit 条，新→旧）。"""

    ts: str = Field(description="调用时刻（本地 naive ISO，毫秒）")
    model: str = Field(default="", description="模型名")
    purpose: str = Field(default="", description="main | compact")
    prompt: int = Field(default=0, description="prompt_tokens（provider 报告）")
    cached: int = Field(default=0, description="cached_tokens（provider 报告）")
    expected: int | None = Field(
        default=None, description="理论可命中 token；null = 无参照"
    )
    verdict: str = Field(default="", description="缓存判定（contract §4）")
    fp: str = Field(default="", description="本次调用的前缀指纹")


class SessionCacheResponse(BaseModel):
    """GET /api/session/cache 响应。

    会话不存在 / 无账本同样是 200 + 空 `forks` / `calls` + 全 0 summary
    （**不 404**）：前端与 CLI 都按同一条路径渲染"还没数据"。
    """

    session_id: str = Field(description="目标 session ID")
    summary: CacheSummary = Field(default_factory=CacheSummary, description="账本汇总")
    forks: list[CacheForkRow] = Field(
        default_factory=list, description="有分叉的调用（新→旧，最多 limit 条）"
    )
    calls: list[CacheCallRow] = Field(
        default_factory=list, description="最近 limit 条调用（新→旧）"
    )


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


class ArchiveResponse(BaseModel):
    """POST /api/session/archive 响应——设置归档状态的结果。

    ``archived`` 是**操作后的真实状态**（不是请求意图）：空会话的归档是 no-op，
    回执如实给 false。``changed`` 说明本次调用是否真的移动了会话（false = 幂等
    no-op：归档已归档的 / 恢复未归档的 / 归档一个从未落盘的空会话）。
    ``detail`` ∈ {archived, already archived, restored, not archived,
    nothing to archive}。
    """

    ok: bool = Field(default=True, description="操作是否成功")
    session_id: str = Field(description="目标 session ID")
    archived: bool = Field(description="操作后的归档状态（true = 在归档区）")
    changed: bool = Field(
        description="本次调用是否真的移动了会话（false = 幂等 no-op）"
    )
    detail: str = Field(
        description=(
            "结果说明（archived / already archived / restored / not archived / "
            "nothing to archive）"
        )
    )


class ArchivedSessionsResponse(BaseModel):
    """GET /api/session/archive 响应——归档区条目（读索引，不扫会话目录）。"""

    sessions: list[ArchivedSession] = Field(
        default_factory=list, description="归档条目（按最后交互时间降序）"
    )
