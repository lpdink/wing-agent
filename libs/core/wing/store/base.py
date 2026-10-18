"""
wing/store/base.py — 持久化层抽象。

两个核心抽象：
- SessionStore: 一个 session 全部持久状态（metadata + 消息日志 + aux）的唯一所有者
- MessageLog: 消息日志的耐久语义（append-only 记录 + 活跃链快照 + aux kv）

关系（组合，非继承）：
- SessionStore 组合 MessageLog 工厂（open_log）
- TrackedList 组合 MessageLog 做 I/O 委托，自身只管链拓扑
- 除本包实现外，任何模块不得直接对存储介质做 I/O

接口刻意保持存储无关（无 path/fsync/glob 概念泄漏），
使 SQLite/PG/Supabase/Redis 后端可以极小成本实现。
"""

from __future__ import annotations

import os
import re
from abc import ABC, abstractmethod
from contextlib import contextmanager
from typing import Any, Iterator

from pydantic import BaseModel, ConfigDict, Field, field_validator

from wing.common.utils import InvalidInputError, is_utf8_encodable
from wing.media import media_id as compute_media_id

_MEDIA_ID_PATTERN = re.compile(r"^[0-9a-f]{64}$")


def validate_media_id(media_id: str) -> str:
    """校验 media id 为内容 sha256（64 位小写 hex）并原样返回。

    这是防路径穿越的唯一防线：media id 直接作为存储路径组件，任何其它
    输入都拒绝——不做"清洗"或"容错"（脏 id 说明调用方逻辑已错，静默归一
    只会把 bug 藏起来）。
    """
    if not isinstance(media_id, str) or _MEDIA_ID_PATTERN.fullmatch(media_id) is None:
        raise ValueError(
            f"invalid media id: {media_id!r} (expect 64 lowercase hex chars)"
        )
    return media_id


FILE_NAME_MAX_BYTES = 128
"""上传文件名长度上限（**字节**，不是字符）。

与 session id 同源的理由（``NAME_MAX`` = 255 字节）但更保守：文件名前面还要挂
``stem-2`` 这类冲突后缀，且它只作展示与 agent 识别用，128 字节远超真实文件名。
按字节计 = 中文名合法（``"报告.pdf"`` 是 10 字节），不会在 ``ENAMETOOLONG`` 上炸。
"""


def is_valid_file_name(name: object) -> bool:
    """判断值是否可作为上传文件名（不抛异常，供调用方做闸门判断）。

    规则与 session id 同**精神**不同**实现**（文件名的语义不同：允许 ``-`` 开头、
    允许空格与中文，但仍拒绝点开头——保留存储的点命名空间与"隐藏文件"的歧义）：
    非空、可编码为 UTF-8（孤立代理字符不能）、≤ :data:`FILE_NAME_MAX_BYTES` 字节、
    不含路径分隔符（``/`` ``\\``）或父目录引用（``..``）、不以 ``.`` 开头、
    不含 ASCII 控制字符（0x00–0x1F / 0x7F）。

    拒绝而非清洗：名字是**路径组件**（防穿越），也是一份**用户标识**（agent 通过
    它认得这个文件）——静默归一（替换 / 截断）会让实际落盘名与用户给的名字悄悄
    对不上，违背"响应里的 name 永远是实际落盘名"的契约。
    """
    if not isinstance(name, str) or not name:
        return False
    if not is_utf8_encodable(name):
        return False
    if len(name.encode("utf-8")) > FILE_NAME_MAX_BYTES:
        return False
    if name.startswith("."):
        return False
    if "/" in name or "\\" in name or ".." in name:
        return False
    return not any(ord(ch) < 0x20 or ord(ch) == 0x7F for ch in name)


def validate_file_name(name: str) -> str:
    """校验上传文件名并原样返回；不合规即 ``InvalidInputError``（HTTP 400 语义）。

    错误文案里的名字用 ``repr``（转义形式，恒为 ASCII）：孤立代理字符直接拼进
    消息会在 HTTP 错误体序列化处再次炸（与 ``validate_session_id`` 同款）。
    """
    if not is_valid_file_name(name):
        raise InvalidInputError(
            f"invalid file name: {name!r} "
            f"(expect non-empty UTF-8 encodable name, ≤ {FILE_NAME_MAX_BYTES} bytes, "
            "no path separator / '..' / control char / leading '.')"
        )
    return name


def disambiguated_file_name(name: str, index: int) -> str:
    """同名异字节冲突时的候选名：``stem-<index>.ext``（无扩展名 ``name-<index>``）。

    候选名同样过名字闸门——**加后缀后超长**（原名已顶到字节上限）即
    :class:`InvalidInputError`，文案点明是"冲突后缀把名字顶出上限"而不是调用方
    给的名字本身脏了（原名合法，是"重名 + 超长"这个组合没有合法解）。**不截断**
    stem：静默改名比一次大声失败更糟（那是把用户的标识符悄悄改样）。
    """
    stem, suffix = os.path.splitext(name)
    candidate = f"{stem}-{index}{suffix}"
    if not is_valid_file_name(candidate):
        raise InvalidInputError(
            f"file name too long to disambiguate: {name!r} is already taken by "
            f"different bytes, and the suffixed name {candidate!r} exceeds the "
            f"{FILE_NAME_MAX_BYTES} byte limit"
        )
    return candidate


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


def validate_media_content(media_id: str, data: bytes) -> None:
    """校验媒体 id 与其字节一致（内容寻址完整性）。

    只在**首写路径**调用（对象已存在时跳过——同 id 必然同内容，跳过省一次
    全量哈希）。不一致说明调用方算错了 id 或传错了 data，必须大声失败：
    否则读回的字节与引用元数据（id/尺寸）永久错位，且没有任何告警。
    """
    actual = compute_media_id(data)
    if actual != media_id:
        raise ValueError(
            f"media content-address mismatch（内容寻址不一致）: "
            f"id={media_id} 与 sha256(data)={actual} 不符"
        )


class SessionUsage(BaseModel):
    """会话用量与运行时长的累计账本（session 维度，跨进程延续）。

    落 ``metadata.json`` 的 ``usage`` 字段，经 ``/api/session/info`` 与
    ``sync_session`` 快照对前端开放——它是「本会话自身花了多少」的持久账本，
    与 audit / metrics.json 审计链路无关（那套将来重做，互不读写）。

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


IMAGE_ONLY_TITLE = "[image message]"
"""会话标题回退：链上只有纯图 user 消息（content 为空、内容全在 media）时的占位名。

没有它，这类会话的标题回退为空 → 上层（``SessionManager.list_sessions``）把
"无名且无标"的会话隐藏，用户刚建的贴图会话在列表里凭空消失（02 之前不可达：
纯图消息会被 ``run_turn`` 丢弃；之后是正常可达状态）。
"""


def first_user_title(records: Iterator[dict[str, Any]]) -> str | None:
    """会话标题回退：第一条**非空文本** user 记录（截断 100 字符）；没有文本但有
    携带 media 的 user 记录时回退为 ``IMAGE_ONLY_TITLE``；两者都没有 → None。

    跳过空 content 的 user 记录继续往后找——"图片先行"的会话不该因为第一条
    用户消息没有文本而在列表里无名。两个后端共用本函数（同一条规则只有一份实现）。
    """
    image_only = False
    for record in records:
        if record.get("role") != "user":
            continue
        text = str(record.get("content") or "")[:100]
        if text:
            return text
        image_only = image_only or bool(record.get("media"))
    return IMAGE_ONLY_TITLE if image_only else None


#: assistant 记录的派生字段：``content_blocks`` 的派生物，落盘恒不写
#: （``Message`` 的访问器实时计算，见 :func:`canonical_record`）。
_DERIVED_FLAT_FIELDS = ("content", "reasoning_content", "tool_calls")


def canonical_record(record: dict[str, Any]) -> dict[str, Any]:
    """history 记录的**规范形**——落盘前必过（规则唯一实现，新 dict 返回）。

    两条规则，都只服务"落盘只存事实不存副本"。

    - **null 不写**：缺席即默认。``0`` / ``False`` / ``""`` 照常写（显式建模，
      见链模型改造的硬约束）。走 ``model_dump`` 的调用方已用
      ``exclude_none=True`` 处理 model 字段（含嵌套）；这里的顶层过滤**主要
      服务 fork 的原始 dict 复制**（`SessionManager._remap_record_uuids`，不经
      pydantic），顺带兜住任何要落盘的 dict。
    - **assistant 的派生字段不写**：``content`` / ``reasoning_content`` /
      ``tool_calls`` 由 ``content_blocks`` 派生。**只在该记录带 content_blocks
      时剥**——没有 blocks 的存量记录（develop 基线格式）里扁平字段是唯一
      存储，必须原样保留。

    自由载荷（工具参数 ``input`` / ``arguments``、``questions`` 等）里的 null
    是**数据**不是缺席，一律不动——它们不在 pydantic 模型字段上，天然不受
    ``exclude_none`` 影响；本函数只做顶层过滤，不下钻。

    读取侧的 ``Message`` 加载路径对两种形态都接受（``Message._route_flat``：
    blocks 在则扁平字段忽略，blocks 缺则由扁平值重建），因此**不需要迁移存量
    文件**。但**直接读 raw record 的辅助路径**（标题回退、fork 的 draft）要
    自己按 blocks 派生——它们看到的是记录，不是消息。
    """
    canonical = {key: value for key, value in record.items() if value is not None}
    if canonical.get("role") == "assistant" and "content_blocks" in canonical:
        for key in _DERIVED_FLAT_FIELDS:
            canonical.pop(key, None)
    return canonical


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


class MessageLog(ABC):
    """消息日志的耐久语义。传输单位为 raw dict 记录（schema 归 TrackedList 管）。

    耐久语义由后端定义：file 后端 fsync，memory 后端进程内，
    SQL 后端即一张 (session_id, seq, record jsonb) 表。

    **记录契约**：记录及其嵌套值对读取方**只读**——实现可以直接交出内部
    结构（memory 后端即如此），消费方不得就地改写；需要变形时自己拷贝
    （如 ``SessionManager._remap_record_uuids`` 的顶层浅拷贝）。
    """

    @abstractmethod
    def iter_all(self) -> Iterator[dict[str, Any]]:
        """按写入序**流式**迭代全部记录。无记录为空迭代。

        流式是接口契约而非实现细节：历史日志动辄 MiB 级，加载路径
        （``TrackedList.load``）边读边构造类型化对象，峰值内存不再是
        "整份 raw dict 列表 + 类型化对象同时在世"的量级。需要列表的
        调用方自行 ``list()``，但不要用它当默认姿势。

        损坏的记录行由后端跳过（存储完整性归后端管）：无法解析的行、以及
        能解析但不是 dict 的行都不产出——消费方可以假定每条记录都是 dict。
        """

    def seal(self) -> None:
        """封住**本实例**的写口（会话被逐出时调用；默认 no-op）。

        封的是"迟到的写"：会话摘除后，任何落盘都会把 ``<root>/<sid>/`` 连
        父目录一起重建（文件后端写前 mkdir），与归档 rename 叠加即 live 与
        归档区**双存在**的数据分裂。判据只在当前实例上生效——重新水合的会话
        拿到的是新实例（`SessionStore.open_log` 新建）。
        """
        return None

    @abstractmethod
    def append(self, records: list[dict[str, Any]]) -> None:
        """批量追加记录（append-only，永不修改已有记录）。空列表为 NOP。"""

    @abstractmethod
    def read_aux(self, key: str) -> dict[str, Any] | None:
        """读取辅助数据（与消息日志同生命周期，如 pending_compact）。

        不存在返回 None。损坏数据由后端丢弃（返回 None）。
        key 必须是文件名安全的内部常量。
        """

    @abstractmethod
    def write_aux(self, key: str, data: dict[str, Any]) -> None:
        """写入辅助数据。"""

    @abstractmethod
    def delete_aux(self, key: str) -> None:
        """删除辅助数据。不存在为 NOP。"""


class SessionStore(ABC):
    """一个 session 全部持久状态的唯一所有者。

    实现：FileSessionStore（现有文件布局）、MemorySessionStore（不落盘）。

    **session id 契约**：id 由会话层确定——默认后端自生成
    （``common.utils.generate_session_id`` → ``YYYYMMDD-HHMMSS-<8hex>``），
    亦可由编排方自带（``POST /api/session/create`` 的 ``session_id`` =
    create-or-adopt）。所有后端入口（exists / metadata / log）都在拼接或
    取值前过闸门：只拒绝路径穿越与卫生问题（`/`、反斜杠、`..`、点开头、
    ASCII 控制字符、超长、空串），其余 UTF-8 一律是合法 id——它是路径组件
    或键，脏值必须大声失败（同 ``validate_media_id`` 的精神）。对网络请求的
    **not-found 语义** 在会话层完成：解析入口先把不合规 id 折成"不存在"
    （404），存储层的 ValueError 只服务编程错误。
    """

    durable: bool = True
    """持久性：True = 状态跨进程存活（逐出后可水合回来）。

    MemorySessionStore 置 False——对它而言"逐出"等于数据销毁，
    SessionManager 的逐出判定会跳过非持久后端的会话。
    """

    @property
    @abstractmethod
    def name(self) -> str:
        """后端名称（API 响应回显、create_session 的 backend 参数）。"""

    @abstractmethod
    def load_metadata(self, session_id: str) -> SessionMetadata | None:
        """加载元数据。无记录返回 None。"""

    @abstractmethod
    def save_metadata(self, session_id: str, metadata: SessionMetadata) -> None:
        """保存元数据。

        全 None（序列化后无字段）且**尚无现存记录**时跳过——首次写入不创造
        空记录；已有记录时照写（可能写成空记录，即"清空"是显式可持久化的：
        标签增删等"字段级清空"操作不能因整体变空而被静默吞掉）。
        """

    @abstractmethod
    def open_log(self, session_id: str) -> MessageLog:
        """打开 session 的消息日志句柄。"""

    @abstractmethod
    def list_summaries(self) -> list[SessionSummary]:
        """列举所有有消息的 session。"""

    @abstractmethod
    def exists(self, session_id: str) -> bool:
        """该 session 是否存在（精确匹配 session id）。"""

    @abstractmethod
    def resolve_stored_id(self, session_id: str) -> str | None:
        """把请求的 session id 解析为存储里的**真实键**；毫无痕迹时返回 None。

        与 :meth:`exists` 的分工：``exists`` 问"这是不是一个会话"，本方法问
        "这个 id 在存储里对应**哪一个键**"（file 后端 = 目录名，memory 后端 =
        字典键）。返回"键"而不是布尔，是因为键**可能与请求值不同**：

        - 大小写不敏感（macOS APFS 默认）或 Unicode 归一化不敏感的文件系统上，
          ``team-a`` 与 ``Team-A`` 解析到同一份日志；
        - 目录**存在但还不是会话**（失败的 create 残留 / 手工目录）也要被看见，
          否则新建会把写入目标落到别人的目录名上。

        会话层据此把内存索引与落盘键对齐——**内存键 == 存储键**是"两个不同 id
        不会共用一份 history.jsonl"的唯一保证（静默数据混合的红线）。调用方
        仍需自行判断"是不是会话"（``exists``），本方法只回答"键是哪个"。

        实现契约：不合规的 id 抛 ``ValueError``（同其它入口，闸门先于一切）；
        返回值一律是**能通过闸门的键**。
        """

    @abstractmethod
    def claim_session_id(self, session_id: str) -> str:
        """为**新建**会话认领键，返回该会话在本存储里的键（= 内存索引的键）。

        与 :meth:`resolve_stored_id` 的差别是"能不能回答还不存在的东西"：
        别名在**目录还没有**时无法被看见（``team-a`` 与 ``Team-A`` 都还没有目录，
        谁也不知道会不会撞上），而两个空会话各自键在自己的拼写上，一落盘就会
        **共用一份日志**。file 后端因此在这里做一次 ``mkdir``：EEXIST ⇒ 该路径
        已经解析到某个目录（别名或残留目录）→ 取真实目录名；成功 ⇒ 本文件系统
        视两者为不同目录（Linux 的 ext4/btrfs），逐字使用。多出来的空目录是
        "会话正在被创建"的正常前奏（空目录不算会话，见 ``exists``）。

        调用点只有 ``SessionManager.create_session`` 的新建路径：读路径（resume /
        tag / send / release）一律用纯解析的 :meth:`resolve_stored_id`，绝不因为
        查一个不存在的 id 而建目录。

        实现契约：不合规的 id 抛 ``ValueError``；返回值是**能通过闸门的键**。
        """

    # ── 归档区（archive）────────────────────────
    #
    # 归档 = 把会话从 live 区移入后端的「归档区」（file 后端 = <root>/.archive/，
    # 点前缀是存储保留命名空间，永远不可被寻址成会话）。语义是**冷存储**：
    # 归档后的会话不参与列表 / 解析 / 水合，恢复必须显式 unarchive。
    # 归档只移动目录、不改任何字节；`.archive` 名与 <root> 自身同在，因此
    # rename 恒在同一文件系统上（原子）。
    #
    # 能力位 `archivable` 是调用方（SessionManager）的唯一判据：不具备该能力的
    # 后端（memory）在显式归档时 409 拒绝、在自动 sweep 时被跳过。

    archivable: bool = False
    """归档能力：True = 支持把会话移入归档区（archive/unarchive/list_archived）。

    MemorySessionStore 不置位——进程内数据没有冷热之分（重启即消失），
    "归档"只是一种更隐蔽的销毁。
    """

    def resolve_archived_id(self, session_id: str) -> str | None:
        """把请求的 session id 解析为**归档区**里的真实键；不在归档区返回 None。

        与 :meth:`resolve_stored_id` 同一精神（别名归一、逐字比对真实目录名），
        作用域换成归档区：归档区内的 id 同样不能被两个拼写各写一份。没有归档区
        的后端恒返回 None。不合规 id 抛 ValueError（闸门先于一切）。
        """
        return None

    def is_archived(self, session_id: str) -> bool:
        """该 id 是否在归档区（纯查询，不涉 live 区）。"""
        return self.resolve_archived_id(session_id) is not None

    def archive(self, session_id: str) -> None:
        """把会话移入归档区并登记索引（**同一次调用内完成**，单线程下原子）。

        调用契约：
        - ``session_id`` 必须是 live 区的**真实键**（调用方先经
          :meth:`resolve_stored_id` 归一并确认存在），且调用时归档区没有同名会话；
        - live 目录不存在 → ``FileNotFoundError``（调用方逻辑错误的防御）；
        - live 与归档区**同时存在**（手工拷贝等异常态）→ ``RuntimeError``
          （不猜谁对，交由人裁决）。

        实现契约：目录移动是纯 rename（字节不变）；索引写入失败不阻断归档
        （目录是事实源，索引缺失时列表按目录重建）。

        与 live 区同一假设：sessions 根由**单网关进程**独占。索引是读改写派生
        数据，多进程并发写会互相覆盖（目录移动本身是原子 rename，事实不丢，
        索引可由重建自愈）；跨进程 / 跨机共享同一根不在支持范围内。
        """
        raise NotImplementedError(f"{self.name} does not support archiving")

    def unarchive(self, session_id: str) -> None:
        """把会话移回 live 区并删除索引条目（幂等：条目缺失即跳过）。

        调用契约：``session_id`` 必须是归档区的**真实键**（调用方先经
        :meth:`resolve_archived_id` 归一）且 live 区没有同名会话。

        Raises:
            FileNotFoundError: 归档目录不存在（调用方逻辑错误）。
            RuntimeError: live 区已存在同名会话（不覆盖，交由人裁决）。
        """
        raise NotImplementedError(f"{self.name} does not support archiving")

    def is_listed_session(self, session_id: str) -> bool:
        """该 id 是否算「会话」（**展示判据**：与 :meth:`list_summaries` 同一口径）。

        比 :meth:`exists`（metadata 或 history；服务解析 / 水合）窄一档：只有
        "有 history 或带标签的 metadata"的目录会被列出。归档准入用它——归档一个
        从不出现在任何列表里的目录，只会在索引重建后留下不可发现的条目。
        默认回落到 :meth:`exists`（后端未区分两者时的安全近似）。
        """
        return self.exists(session_id)

    @contextmanager
    def index_batch(self) -> Iterator[None]:
        """归档索引的批量写窗口（批量 sweep 专用；默认 no-op）。

        块内每次归档 / 恢复**照常即时落目录**（rename 的原子性不变），索引
        更新攒到块尾一次落盘——批量 sweep 不做这件事的话，n 条候选就是 n 次
        "读整份索引 + 写整份索引 + fsync"（O(n²)，同步块还会卡住事件循环）。
        代价是"目录已移动但索引未登记"的窗口随块长放大：列表读取会当场收编
        （目录是事实源），窗口不落盘也不丢事实。
        """
        yield

    def list_archived(self) -> list[ArchivedSession]:
        """列出归档条目（读索引；索引缺失 / 损坏时按归档目录重建）。

        保证（目录是事实源、索引只是加速）：条目对应的归档目录必须存在
        （手工删除的不投影）；目录存在但索引未登记的（手工拷入 / 索引写失败的
        窗口）被当场收编。索引整份损坏时按目录重建（``archived_at`` 留空）。
        """
        raise NotImplementedError(f"{self.name} does not support archiving")

    # ── 媒体字节（内容寻址，**session 私有**）──────
    #
    # 媒体对象是会话的私有资产：file 后端落 <root>/<sid>/media/<id>，memory
    # 后端是"实例内 per-session 键空间"——session 目录自此**自包含**（整个
    # 目录拷贝到另一台机器后历史引用全部可用，不依赖任何共享池）。会话消息
    # 只持有 MediaRef 引用，字节随会话可恢复性走（durable 后端跨进程存活，
    # memory 后端重启即消失，与既有语义一致）。
    #
    # 兼容：老布局的共享池（file 后端 <root>/.media/<id[:2]>/<id>）**只读不删**
    # ——读路径 miss 会话目录时回退老池并**物化**（复制进会话目录）。写路径
    # 一律只写会话目录。没有任何删除 API，也不清理老池（媒体永不删除）。

    @abstractmethod
    def write_media(self, session_id: str, media_id: str, data: bytes) -> None:
        """把媒体字节写入**该 session 的**媒体目录（内容寻址，幂等）。

        media_id 必须是 64 位小写 hex 的内容 sha256、session_id 必须过会话
        闸门，否则抛 ValueError（两者都是路径组件，防穿越）。**首写路径**还
        校验 id 与字节一致（``validate_media_content``，内容寻址完整性）；
        已存在对象直接跳过（同 id 必然同内容，跳过省一次全量哈希）。I/O 失败
        由后端抛出（调用方决定是否降级）。
        """

    @abstractmethod
    def read_media(self, session_id: str, media_id: str) -> bytes | None:
        """读取媒体字节：session 目录优先，miss 回退老池并**物化**。

        合法 id 但两处都缺失返回 None；非法 id / session_id 抛 ValueError。
        读取 I/O 失败（权限/损坏）按"读不到"处理：WARN + None——序列化侧据此
        降级为占位文本，绝不打断模型请求。回退命中的**物化是 best-effort**：
        复制失败只 WARN，本次读取照常返回字节（下次再试，幂等）。
        """

    @abstractmethod
    def media_exists(self, session_id: str, media_id: str) -> bool:
        """该媒体对象是否可寻址（session 目录或老池任一存在）。**无副作用**。

        供消息入口校验（引用的对象必须可兑现）：只回答"在不在"，不物化
        （存在性查询不该产生写）。
        """

    @abstractmethod
    def media_path(self, session_id: str, media_id: str) -> str | None:
        """该媒体对象的**本地绝对路径**（session 目录优先、老池兜底）；不可寻址返回 None。

        供无视觉能力时的占位文案（"图片保存在哪"）。**纯查询，不物化**——物化
        时机统一归加载路径与读路径。没有本地文件的后端（memory）恒返回 None，
        调用方自行降级（不带路径的占位文本）。
        """

    @abstractmethod
    def materialize(self, session_id: str, media_id: str) -> bool:
        """把老池对象幂等物化进该 session 的媒体目录。

        返回 ``True`` = 调用后 session 目录内已有该对象（本来就在，或刚从老池
        复制成功）；``False`` = 两处都缺或复制失败（实现已 WARN）。先 stat 目标
        （快路径，命中即返回），miss 才碰老池——加载路径的对象量级 = 单会话引用
        数。不含内容校验：老池对象在首写时已校验，物化只保证"读回什么就存什么"
        （与读路径等价）。
        """

    # ── 用户文件（按名寻址，**session 私有**）──────
    #
    # 与媒体（内容寻址）刻意不同的两点：**按名寻址**（名字本身是给 agent 与用户
    # 的语义——保留原名，agent 的 Read 才看得懂）与**允许同名多份**（字节不同即
    # 递增后缀，绝不覆盖：链上的 `[file: …]` 引用指向既有那份，覆盖 = 静默数据损坏）。
    # 落点 <root>/<sid>/files/<实际名>——与 media/ 平级，随会话目录一起搬运 / 归档，
    # 会话自包含 doctrine 不变。没有删除 / 列目录 / 读取 API（与媒体同 doctrine：
    # 只增不减，目录本身就是事实源）。

    @abstractmethod
    def write_file(self, session_id: str, filename: str, data: bytes) -> FileRef:
        """把用户文件写进**该 session 的文件目录**，返回**实际落盘**引用。

        契约：
        - 文件名过 :func:`validate_file_name` 闸门（路径组件，防穿越；不合规抛
          ``InvalidInputError``）；``session_id`` 同样过会话闸门；
        - **同名同字节幂等**：同名对象已存在且字节全等 → 直接返回既有引用
          （不重写：inode / mtime 不动）；逐候选比较的语义同样适用于冲突后缀名
          （``name-2.ext`` 已存在且同字节 → 复用它，不产生第三份）；
        - **同名异字节递增**：``stem-2.ext`` / ``stem-3.ext``…（无扩展名
          ``name-2``）；加后缀后过不了名字闸门（超长）→ ``InvalidInputError``
          （不静默截断 stem——那是把用户的标识符悄悄改样）；
        - 字节闸门（非空 / 上限）**不在这一层**：上限来自配置，store 不读配置
          （见 ``SessionManager.upload_file``）；本方法假定 ``data`` 已被放行；
        - 返回的 ``FileRef.name`` 是实际落盘名（后缀可能被加过），``path`` 是
          本地绝对路径（memory 后端 None）。
        """
