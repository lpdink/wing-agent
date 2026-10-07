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

import re
from abc import ABC, abstractmethod
from typing import Any, Iterator

from pydantic import BaseModel, ConfigDict, field_validator

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


class SessionMetadata(BaseModel):
    """Session 持久元数据。全字段可选，序列化时排除 None。

    model_name / provider_name 记录会话的模型绑定，二者成对写入、成对读取
    （任一为 None 视为无记录）。写入时机是**显式模型动作**——模型切换、
    模板切换、创建 override、fork 快照；resume 时记录优先于模板默认模型，
    是模型选择跨进程重启的唯一恢复来源。model_name 与 AgentInfo.model_name
    同义（当前生效模型的裸名）。

    提示词与动态状态（system_prompt / append_system_prompt / tools / thinking /
    reasoning_effort / yolo / max_turns）走同一套"显式动作写入、resume 优先
    于模板/配置"语义。它们全部进入 LLM 请求（或改变请求行为），丢失会让
    fork/resume 后的请求前缀与重启前不一致——直接表现为 KV cache 不命中，
    因此必须随会话持久化：

    - system_prompt：基础系统提示词的替换值（AgentOverride.system_prompt）；
    - append_system_prompt：追加系统提示词（hook 注入的环境信息 +
      AgentOverride.append_system_prompt 的合并结果）；
    - tools：可执行工具集覆盖（ref 列表，如 "Bash" / "client.Read"）；
    - thinking / reasoning_effort / yolo / max_turns：会话级开关与限额。

    fork 时这些字段一次写全（快照语义，与模型绑定一致）——子会话重启后不会
    偏离 fork 时的行为：提示词 / 工具集 / yolo / max_turns 取 fork 时刻的
    **有效值**（子会话 agent 由 `AgentTemplate.from_agent` 按 live 构造，记录
    必须与之一致；append_system_prompt 先按 live 值写入供子会话构造时继承，
    随后 `before_session_start` 在新会话上生效、注入结果覆盖落盘）；
    thinking / reasoning_effort 只拷**显式记录**——固化了 provider 派生默认
    （如 anthropic 未配置 thinking）会让子会话请求体带上源会话没有的显式配置。

    - tags：会话级结构化标签（不透明字符串列表，如 ``scheduler`` /
      ``favorite`` / ``task=wing-tag``；约定小写、``k=v`` 作命名空间，系统
      不做语义解析）。由 ``SessionManager.set_session_tags``（``POST
      /api/session/tag``）原子增删维护；不进 LLM 请求前缀，**fork 不继承**。
      空列表与 None 等价（序列化排除 None——无标签的 metadata.json 零字段）。
    - tag_meta：每个标签的元数据记录（``{tag: TagMeta}``），与 tags 同生同灭、
      **键集 ⊆ tags**（不属于 tags 的键在读取投影与下次写盘时丢弃）。由标签
      变更原语（``session.tags.apply_tag_ops``）维护：tag 被添加时记录一次
      加入时间，移除时删除记录。值是对象（当前只有 ``added_at``），**面向
      增量**：后续要加审计字段（来源 / actor / 变更历史）时往 ``TagMeta`` 里
      加即可，不必再动存储结构。空字典与 None 等价（与 tags 同一约定）。
    """

    model_config = ConfigDict(extra="ignore")

    session_name: str | None = None
    workspace: str | None = None
    last_interaction: str | None = None
    forked_from: str | None = None
    template_name: str | None = None
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

    first_user_message: 快照中第一条用户消息（截断至 100 字符），
    仅在 metadata.session_name 为 None 时由后端填充，供上层做标题回退。
    """

    id: str
    metadata: SessionMetadata
    first_user_message: str | None = None


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

    # ── 媒体字节（内容寻址的会话媒体池）──────────
    #
    # 媒体池按"存储根"共享：file 后端多个 session 共用 <root>/.media/，
    # memory 后端同一个 store 实例共享——会话消息只持有 MediaRef 引用，
    # 字节随会话可恢复性走（durable 后端跨进程存活，memory 后端重启即
    # 消失，与既有语义一致）。

    @abstractmethod
    def write_media(self, media_id: str, data: bytes) -> None:
        """写入媒体字节（内容寻址，幂等：已存在即跳过）。

        media_id 必须是 64 位小写 hex 的内容 sha256，否则抛 ValueError
        （防路径穿越）。**首写路径**还校验 id 与字节一致
        （``validate_media_content``，内容寻址完整性）；已存在对象直接
        跳过（同 id 必然同内容，跳过省一次全量哈希）。I/O 失败由后端抛出
        （调用方决定是否降级）。
        """

    @abstractmethod
    def read_media(self, media_id: str) -> bytes | None:
        """读取媒体字节。合法 id 但对象缺失返回 None；非法 id 抛 ValueError。

        读取 I/O 失败（权限/损坏）按"读不到"处理：WARN 日志 + 返回 None
        ——序列化侧据此降级为占位文本，绝不打断模型请求。
        """
