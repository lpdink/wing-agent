# wing/store/models.py
"""持久化模型：metadata / 用量账本 / 标签记录 / 列表与归档条目。

这些类是**落盘格式与协议投影的共同事实**（``metadata.json`` 的字段名即响应字段
名），字段改动同时改变两者。
"""

from __future__ import annotations

from pydantic import BaseModel, ConfigDict, Field, field_validator


class FileRef(BaseModel):
    """一份落盘文件的引用（``SessionStore.write_file`` 的返回值 / 上传端点的响应元素）。

    - ``name`` 是**实际落盘名**：同名异字节冲突时带 ``-2`` / ``-3`` 后缀——调用方
      永远不要拿请求里的名字当事实（响应与磁盘以本字段为准）。
    - ``path`` 是**网关宿主机上的绝对路径**（消费方把它写进消息正文，agent 用
      Read / Bash 直接读）；没有本地文件的后端（memory）恒为 None，如实降级
      （与 ``media_path`` 同款——内存里的字节读不到就没有路径可给）。
    - ``bytes`` 是本次写入的字节数（= 请求体字节数；同名同字节的幂等命中也是它）。
    """

    name: str
    path: str | None = None
    bytes: int


class SessionUsage(BaseModel):
    """会话用量与运行时长的累计账本（session 维度，跨进程延续）。

    落 ``metadata.json`` 的 ``usage`` 字段，经 ``/api/session/info`` 与
    ``sync_session`` 快照对前端开放——它是「本会话自身花了多少」的持久账本，
    与 audit（wing.db 的 audit_* 追加行）链路无关：那是进程级旁路审计，互不读写。

    字段语义是对外契约，客户端据此推导命中率与工作时长：

    - 计数：``user_messages`` 按被消费进上下文的用户消息计（一轮合并输入逐条
      计）；``llm_calls`` 每次完成的 LLM 调用计一次（含随后被判无效重试的
      尝试——token 真花掉了），**只覆盖 ReAct 循环内的调用**：上下文压缩
      （compaction）与其它旁路调用不计（它们的 token 也不进本账本）；
      ``tool_calls`` 每次**真实执行**的工具调用计一次——参数非法 / 未知工具
      两条短路与中断合成的结果都不计；``turns`` 每个收尾的回合（发出
      turn_result）计一次；
    - token：``prompt_tokens`` / ``completion_tokens`` / ``cached_tokens``
      与直播 ``llm_call_metrics`` 事件逐字段同源（口径同 ``llm_calls``）；
    - 时长：``llm_time_ms`` 各次 ReAct 调用（从发起到 usage 帧）的总时长
      之和，``tool_time_ms`` 各次真实执行时长之和（并行工具按各自时长求和，
      因此两者可能重叠）。工作时长 = ``llm_time_ms + tool_time_ms`` 是客户端
      可直接断言的下界，账本不另存派生值（命中率 =
      ``cached_tokens / prompt_tokens`` 同理）。

    全字段非负整数、缺省 0：旧会话（引入前落盘、字段缺失）读出的就是零，
    不回填、不发明数字。
    """

    model_config = ConfigDict(extra="ignore")

    user_messages: int = 0
    llm_calls: int = 0
    tool_calls: int = 0
    turns: int = 0
    prompt_tokens: int = 0
    completion_tokens: int = 0
    cached_tokens: int = 0
    llm_time_ms: int = 0
    tool_time_ms: int = 0


class SessionMetadata(BaseModel):
    """Session 持久元数据。全字段可选，序列化时排除 None。

    model_id / model_name / provider_name 记录会话的模型身份**三元组**：``model_id`` 是**引用词**
    （∈ 配置声明的 id 空间），resume 以它为唯一入口——命中即用当前映射（name / provider 跟随配置演
    化），未命中才回落到快照；``model_name`` / ``provider_name`` 是写入时刻的**运行期事实快照**
    （发给上游的调用名 + 承载它的 provider），id 被删 / 改时的兜底恢复线索；三者由
    ``Session._persist_model`` 一次写全（整个 metadata 幂等落盘）。``model_id`` 缺失但快照齐全 = 旧记
    录：恢复时经 ``Config.identify`` 反查补 id，内存态与记录不一致才落盘对齐（一次性迁移，不产生写
    噪声）。

    写入时机是**显式模型动作**——模型切换、模板切换、创建 override、fork 快照；resume 时记录优先于
    模板默认模型，是模型选择跨进程重启的唯一恢复来源。model_name 与 AgentInfo.model_name 同义（当前
    生效模型的裸名）。

    提示词与动态状态（system_prompt / append_system_prompt / tools / thinking / reasoning_effort /
    yolo / max_turns）走同一套"显式动作写入、resume 优先于模板/配置"语义：它们全部进入 LLM 请求（或
    改变请求行为），丢失会让 fork/resume 后的请求前缀与重启前不一致——直接表现为 KV cache 不命中，
    因此必须随会话持久化。system_prompt 是基础系统提示词的替换值；append_system_prompt 是追加部分
    （hook 注入的环境信息 + override 的合并结果）；tools 是可执行工具集覆盖（ref 列表，如 "Bash" /
    "client.Read"）；其余是会话级开关与限额。

    fork 时这些字段一次写全（快照语义，与模型绑定一致）——子会话重启后不会偏离 fork 时的行为：提示词
    / 工具集 / yolo / max_turns 取 fork 时刻的**有效值**（子会话 agent 由 `AgentTemplate.from_agent`
    按 live 构造，记录必须与之一致；append_system_prompt 先按 live 值写入供子会话构造时继承，随后
    `before_session_start` 在新会话上生效、注入结果覆盖落盘）；thinking / reasoning_effort 只拷**显式
    记录**——固化了 provider 派生默认（如 anthropic 未配置 thinking）会让子会话请求体带上源会话没有
    的显式配置。

    - tags：会话级结构化标签（不透明字符串列表，如 ``scheduler`` / ``favorite`` / ``task=wing-tag``；
      约定小写、``k=v`` 作命名空间，系统不做语义解析）。由 ``SessionManager.set_session_tags``
      （``POST /api/session/tag``）原子增删维护；不进 LLM 请求前缀，**fork 不继承**。空列表与 None
      等价（序列化排除 None——无标签的 metadata.json 零字段）。
    - tag_meta：每个标签的元数据记录（``{tag: TagMeta}``），与 tags 同生同灭、**键集 ⊆ tags**（不属
      于 tags 的键在读取投影与下次写盘时丢弃）。由标签变更原语（``session.tags.apply_tag_ops``）维
      护：tag 被添加时记录一次加入时间，移除时删除记录。值是对象（当前只有 ``added_at``），**面向增
      量**：后续要加审计字段（来源 / actor / 变更历史）时往 ``TagMeta`` 里加即可。空字典与 None 等价。
    - usage：会话用量 / 运行时长的累计账本（计数 + token + 时长），由 ``session.usage.UsageTracker``
      在回合收尾、会话释放与进程退出前落盘；旧会话没有这个记录，读出即零。语义见 ``SessionUsage``。
    - parent_session_id：创建者会话 id（血缘——谁创建了我，产品语义见 docs/dev/session-lineage.md）。
      **弱引用**：创建时只做格式卫生（同 session id 规则），不校验父是否存在；创建时刻一次写入，此后没有
      修改路径（树是创建时刻的事实）。与 ``forked_from``（内容血统：fork 的源会话）正交——它记录的是
      **创建动作**。收养（create-or-adopt 命中已存在 id）忽略请求里的 parent，绝不改写既有记录。
    - last_turn_end / last_turn_subtype：最近一次**收口轮次**的结束时刻（本地 naive ISO，与
      ``last_interaction`` 同口径）与该轮的 ``TurnResultEvent.subtype``（``success`` /
      ``error_during_execution`` / ``error_max_turns``，逐字透传不收敛）。写点搭回合收口的 usage
      flush 便车（每轮零新增写盘，见 ``Session._on_usage_event``）；下一次收口覆盖，两轮之间反映
      上一轮。**中断不写**（中断沿 CancelledError 路径收场、不发 ``turn_result``——用户主动停止
      不是交付），因此字段只在真的收口过时才有值，旧会话读出 None。跨进程事实：落盘于
      ``metadata.json``，未加载 / 已逐出的会话读列表同样可得。
    """

    model_config = ConfigDict(extra="ignore")

    session_name: str | None = None
    workspace: str | None = None
    last_interaction: str | None = None
    forked_from: str | None = None
    parent_session_id: str | None = None
    template_name: str | None = None
    model_id: str | None = None
    model_name: str | None = None
    provider_name: str | None = None
    system_prompt: str | None = None
    append_system_prompt: str | None = None
    tools: list[str] | None = None
    thinking: bool | None = None
    reasoning_effort: str | None = None
    yolo: bool | None = None
    max_turns: int | None = None
    tags: list[str] | None = None
    tag_meta: dict[str, "TagMeta"] | None = None
    usage: SessionUsage | None = None
    last_turn_end: str | None = None
    last_turn_subtype: str | None = None

    @field_validator("tags", mode="after")
    @classmethod
    def _normalize_tags(cls, value: list[str] | None) -> list[str] | None:
        """空列表与 None 等价（无标签不落字段，存量文件零变化）。"""
        return value or None

    @field_validator("tag_meta", mode="before")
    @classmethod
    def _drop_bad_tag_meta(cls, value: object) -> dict[str, TagMeta] | None:
        """读侧容错：坏键 / 坏值**逐条丢弃**，不让整份 metadata 降级。

        ``tag_meta`` 是嵌套结构，被手改 / 老版本写坏的概率远高于平铺字段；
        一条坏记录就让整个会话的 metadata（含 model / 提示词等）变成空对象
        是不可接受的。这里在模型边界做与 ``session.tags.sanitize_tag_meta``
        同一精神的清洗（丢弃而非 raise）；"键集 ⊆ tags" 的约束不在这里管
        （需要 tags 上下文，由投影与写路径负责）。空字典与 None 等价。
        """
        if not isinstance(value, dict):
            return None
        clean: dict[str, TagMeta] = {}
        for key, raw in value.items():
            if not isinstance(key, str):
                continue
            try:
                clean[key] = (
                    raw if isinstance(raw, TagMeta) else TagMeta.model_validate(raw)
                )
            except Exception:
                continue
        return clean or None


class TagMeta(BaseModel):
    """单个标签的元数据记录（``SessionMetadata.tag_meta`` 的值）。

    **面向增量**：现在是单字段对象，未来加审计维度（来源 / actor / 变更
    历史）时在此扩展，存储结构不必再动。``extra="ignore"`` 保证新版本写入
    的字段不会让旧版本的读取失败（旧版本读新文件时静默丢弃未知字段）。

    added_at: 标签**加入**的时间（``datetime.now().isoformat()``，本地 naive
    ISO——与 ``last_interaction`` 同一时间口径）。重复添加（已存在的 tag）
    是幂等 no-op，**不刷新**这个时间；移除即删除整条记录（清空后整字段
    从 metadata 消失）。缺失 / 不可解析 = 时间未知：读取方（前端排序等）
    自行降级，不影响标签本身的有效性。
    """

    model_config = ConfigDict(extra="ignore")

    added_at: str | None = None


class SessionSummary(BaseModel):
    """session 列举条目：id + metadata + 标题回退素材。

    first_user_message: 快照中第一条**非空文本**用户消息（截断至 100 字符），
    仅在 metadata.session_name 为 None 时由后端填充，供上层做标题回退；
    纯图消息（content 为空、media 非空）回退为 :data:`IMAGE_ONLY_TITLE`。
    """

    id: str
    metadata: SessionMetadata
    first_user_message: str | None = None


class ArchivedSession(BaseModel):
    """归档区条目：索引持久化形态 + 列表投影素材。

    **归档时刻的事实快照**（写入索引时定稿）：标题（含首条消息回退）、tags、
    模型三元组、最后交互时间、归档时间。归档区内的会话是冻结的（不可读写），
    因此这些字段不会过时——列表（``GET /api/session/archive``）读索引即可，
    不扫任何会话目录（归档路径必须保持低频、廉价）。

    ``model_display_name`` 是**投影期补全**的展示素材（不在索引里持久化）：
    展示名随配置演化（改名 / 新增声明），与 ``/api/session/list`` 同一口径，
    在读取时按 ``model_id`` 经配置现算。

    ``archived_at`` 可能为 None：索引重建路径（索引缺失 / 损坏，从目录反推）
    不知道当时的归档时刻，不回填、不发明数字（用目录 mtime 冒充是错的——
    目录 rename 不更新目录自身的 mtime）。

    索引是**派生数据**（目录才是事实源），读写走"整份读 → 改写 → 写回"：
    ``extra="ignore"`` 意味着更新版本写入的未知字段会被旧版本一读一写抹掉
    ——可接受（条目可由目录重建，字段按需再加）。
    """

    model_config = ConfigDict(extra="ignore")

    id: str
    name: str | None = None
    workspace: str | None = None
    template_name: str | None = None
    last_interaction: str | None = None
    archived_at: str | None = None
    tags: list[str] = Field(default_factory=list)
    model_id: str | None = None
    model_name: str | None = None
    provider_name: str | None = None
    model_display_name: str | None = None
