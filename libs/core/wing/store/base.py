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

模型 / 闸门 / 记录规范形分别在 ``store.models`` / ``store.names`` / ``store.records``；
本模块只留两个 ABC（消费方走包根 ``wing.store`` 或对应子模块）。
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from contextlib import contextmanager
from typing import TYPE_CHECKING, Any, Iterator

if TYPE_CHECKING:
    from wing.store.models import (
        ArchivedSession,
        FileRef,
        SessionMetadata,
        SessionSummary,
    )

__all__ = [
    "MessageLog",
    "SessionStore",
]


class MessageLog(ABC):
    """消息日志的耐久语义。传输单位为 raw dict 记录（schema 归 TrackedList 管）。

    耐久语义由后端定义：file 后端 fsync，memory 后端进程内，
    SQL 后端即一张 (session_id, seq, record jsonb) 表。

    **记录契约**：记录及其嵌套值对读取方**只读**——实现可以直接交出内部
    结构（memory 后端即如此），消费方不得就地改写；需要变形时自己拷贝
    （如 ``session.fork._remap_record_uuids`` 的顶层浅拷贝）。
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
