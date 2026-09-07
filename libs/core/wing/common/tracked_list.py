"""
TrackedList — 链拓扑引擎，UUID + parentUuid 链模型。

核心设计：
- TrackedList 只负责内存中的链拓扑（uuid/parentUuid 填充、trace、find、set_tip）
- 所有耐久 I/O 委托给组合的 MessageLog（wing.store.base）；log=None 时纯内存
- 消息以 uuid 为唯一标识，以 parentUuid 构建链拓扑
- 日志语义 append-only（只追加，不修改已有记录）
- 链节点是 ChainNode 家族：Message（LLM 上下文投影）与 WingEvent（事件
  记录，role="event"）混合成链——history.jsonl 单一日志承载两种记录，
  记录级判别靠 role 字段。加载时按 role 分发（事件走 EVENT_TYPES 注册表）。
- 始终以类型化对象工作，存储层传输 raw dict（ts 由本类注入）
- 内存中维护 full chain map（_all_items），find/trace_chain 从内存读取

已知问题：
1. 并发不安全。
2. compact 的 append_detached + set_tip 之间存在崩溃窗口（见 ContextManager
   中的 TODO(crash-safety)）。彻底修复需要 MessageLog 支持批量原子切换 tip。
"""

from __future__ import annotations

import uuid as uuid_mod
from datetime import datetime
from typing import Any, Generic, Iterator, List, Type, TypeVar, Union, overload

from wing.schema import ChainNode
from wing.store.base import MessageLog

T = TypeVar("T", bound=ChainNode)


class TrackedList(Generic[T]):
    """链拓扑引擎 + 可空 MessageLog 委托。log=None 时为纯内存列表。

    类型参数 T 是链的"主类型"标注（session 场景为 ChainNode——Message 与
    事件混排）。Message 投影（过滤事件节点）由 ContextManager 的
    get_context_window() 集中提供。
    """

    def __init__(self, log: MessageLog | None = None) -> None:
        self._log: MessageLog | None = log
        self._data: list[T] = []
        self._last_uuid: str | None = None  # 活跃链最后一条消息的 uuid

        # 内存 full chain map：仅 load / 写操作维护，find/trace_chain 从此读取
        self._all_items: dict[str, T] = {}  # uuid -> item
        self._all_parent_uuids: set[str] = set()
        self._lines_order: list[str] = []

    # ── 属性 ──────────────────────────────────

    @property
    def active_chain(self) -> list[T]:
        """当前上下文窗口（_data 的副本）。"""
        return list(self._data)

    @property
    def last_uuid(self) -> str | None:
        """活跃链末尾消息的 uuid。"""
        return self._last_uuid

    # ── 内部：类型与序列化 ────────────────────

    def _ensure_type(self, items: list[T]) -> None:
        """检查 ChainNode 家族约束，同时填充 uuid/parentUuid。

        混合链：Message 与 WingEvent（均继承 ChainNode）可任意混排。
        """
        for it in items:
            if not isinstance(it, ChainNode):
                raise TypeError(
                    f"expected ChainNode (Message or WingEvent), "
                    f"got {type(it).__name__}"
                )
            # 填充 uuid 和 parentUuid
            if it.uuid is None:
                it.uuid = str(uuid_mod.uuid4())
            if it.parent_uuid is None:
                it.parent_uuid = self._last_uuid
            # 更新 _last_uuid，供下一条消息链接
            if it.uuid:
                self._last_uuid = it.uuid

    def _check_type(self, items: list[T]) -> None:
        """检查 ChainNode 家族约束（detached 场景，不填充拓扑字段）。"""
        for it in items:
            if not isinstance(it, ChainNode):
                raise TypeError(
                    f"expected ChainNode (Message or WingEvent), "
                    f"got {type(it).__name__}"
                )

    @staticmethod
    def _to_record(item: T) -> dict[str, Any]:
        """序列化为存储记录（model_dump + 剥离无信息字段 + ts）。

        剥离三类：
        - `target`：EventBus 注入的传输路由元数据，不属于事实记录
          （Message 无该字段，pop 为 NOP）；
        - `disk_exclude` 列出的字段（事件 ClassVar）：只服务直播、落盘即孪生
          （如 TurnResultEvent.result）——wire 帧仍携带，仅磁盘记录剥离；
        - 值为 null 的字段：**字典推导剥除**，MUST NOT 用
          `model_dump(exclude_none=True)`——`Message._serialize_flat` 是
          `mode="wrap"` 序列化器，content / reasoning_content / tool_calls 在
          内层 handler 跑完之后才注入 dict，exclude_none 看不到它们。

        链拓扑与记录判别字段（role / uuid / parent_uuid / unzip_last_uuid）
        在非 null 时保留。加载路径对"键缺失"与"值为 null"一视同仁
        （TrackedList.load 用 record.get，Message._route_flat 用 pop(...,None)）。
        """
        entry = item.model_dump(mode="json")
        entry.pop("target", None)
        disk_exclude = getattr(item, "disk_exclude", None)
        if disk_exclude:
            for key in disk_exclude:
                entry.pop(key, None)
        entry = {k: v for k, v in entry.items() if v is not None}
        entry["ts"] = datetime.now().isoformat()
        return entry

    # ── 内部：I/O 委托 ────────────────────────

    def _persist_append(self, items: list[T]) -> None:
        """追加记录到 MessageLog。log=None 时 NOP。"""
        if self._log is not None:
            self._log.append([self._to_record(it) for it in items])

    # ── 内部：内存 map 维护 ───────────────────

    def _update_memory(self, item: T) -> None:
        """将一条消息同步到内存 full chain map。"""
        if item.uuid:
            self._all_items[item.uuid] = item
            if item.parent_uuid:
                self._all_parent_uuids.add(item.parent_uuid)
            self._lines_order.append(item.uuid)

    # ── aux 透传 ──────────────────────────────

    def read_aux(self, key: str) -> dict[str, Any] | None:
        """读取辅助数据（如 pending_compact）。log=None 时返回 None。"""
        if self._log is None:
            return None
        return self._log.read_aux(key)

    def write_aux(self, key: str, data: dict[str, Any]) -> None:
        """写入辅助数据。log=None 时 NOP。"""
        if self._log is not None:
            self._log.write_aux(key, data)

    def delete_aux(self, key: str) -> None:
        """删除辅助数据。log=None 时 NOP。"""
        if self._log is not None:
            self._log.delete_aux(key)

    # ── 静态恢复接口 ──────────────────────────

    @classmethod
    def load(cls, log: MessageLog, item_type: Type[T]) -> TrackedList[T]:
        """从 MessageLog 完全恢复状态（冷启动）。

        混合日志按记录级 role 字段分发：
        - role="event" → EVENT_TYPES 注册表按 type 还原事件节点
          （未知事件 type 跳过——前向容忍；校验失败的记录跳过）
        - 其余 → item_type.model_validate（Message）

        1. 将全部记录读入内存 map
        2. 从内存 map 重建活跃链（trace_chain）——事件节点与消息同链
        """
        from wing.event import EVENT_TYPES

        tl = cls(log)

        for record in log.load_all():
            try:
                if record.get("role") == "event":
                    event_cls = EVENT_TYPES.get(record.get("type", ""))
                    if event_cls is None:
                        continue
                    item: ChainNode = event_cls.model_validate(record)
                else:
                    item = item_type.model_validate(record)
            except Exception:
                continue
            tl._update_memory(item)

        tl._data = tl.trace_chain()

        # 从 _data 中恢复 _last_uuid
        for item in tl._data:
            if item.uuid:
                tl._last_uuid = item.uuid

        return tl

    # ── 写操作（日志语义 append-only）─────────

    def append(self, item: T) -> None:
        """追加元素。填充 uuid/parentUuid，写日志。"""
        self._ensure_type([item])
        self._data.append(item)
        self._last_uuid = item.uuid
        self._persist_append([item])
        self._update_memory(item)

    def extend(self, items: Union[Iterator[T], List[T]]) -> None:
        """批量追加多条消息。每条填充 uuid/parentUuid，写日志。"""
        lst = list(items)
        self._ensure_type(lst)
        self._data.extend(lst)
        for item in lst:
            self._update_memory(item)
            self._last_uuid = item.uuid
        self._persist_append(lst)

    def extend_detached(self, items: Union[Iterator[T], List[T]]) -> None:
        """批量追加多条消息，不自动填充 uuid/parentUuid。

        调用方已自行设置好拓扑关系时使用（fork 导入场景）。
        """
        lst = list(items)
        if not lst:
            return
        self._check_type(lst)
        self._data.extend(lst)
        for item in lst:
            self._update_memory(item)
            self._last_uuid = item.uuid
        self._persist_append(lst)

    def append_detached(self, item: T) -> None:
        """追加消息，但不自动填充 uuid/parentUuid。

        调用方已自行设置好拓扑关系时使用（compact/rewind 场景）。
        """
        self._check_type([item])
        self._data.append(item)
        self._last_uuid = item.uuid
        self._persist_append([item])
        self._update_memory(item)

    def set_tip(self, uuid: str) -> None:
        """将活跃链末尾切换到指定 uuid。

        1. 从内存 map 重建以 uuid 为叶的链（沿 parent_uuid 回溯到根）
        2. 用重建结果替换 _data
        3. 更新 _last_uuid = uuid
        """
        chain = self.trace_chain(from_uuid=uuid)
        if not chain and uuid is not None:
            raise ValueError(f"uuid {uuid} not found in history")
        self._data = chain
        self._last_uuid = uuid

    # ── 查询（从内存读取）─────────────────────

    def trace_chain(self, from_uuid: str | None = None) -> list[T]:
        """从内存 map 沿 parent_uuid 回溯构建链（倒序遍历）。

        - from_uuid=None → 从内存 map 的叶节点开始
        - from_uuid=uuid → 从该 uuid 开始
        - 回溯终止条件：parent_uuid is None

        始终以类型化对象工作，返回 [根, ..., 叶] 顺序。
        """
        if not self._all_items:
            return []

        # 确定起点
        if from_uuid is not None:
            start_uuid = from_uuid
        else:
            # 叶节点：uuid 不在 _all_parent_uuids 中的消息（最后出现的）
            leaf_candidates = [
                u for u in self._lines_order if u not in self._all_parent_uuids
            ]
            start_uuid = (
                leaf_candidates[-1] if leaf_candidates else self._lines_order[-1]
            )

        # 从起点倒序遍历到根
        chain: list[T] = []
        current_uuid = start_uuid
        while current_uuid:
            item = self._all_items.get(current_uuid)
            if not item:
                break
            chain.append(item)
            if item.parent_uuid is None:
                break
            current_uuid = item.parent_uuid

        # 反转：从根到叶
        chain.reverse()
        return chain

    def find(self, uuid: str) -> T | None:
        """在内存 map 中按 uuid 查找消息（last-write-wins），返回类型化对象。"""
        return self._all_items.get(uuid)

    def trace_full_chain(self, from_uuid: str | None = None) -> list[T]:
        """从内存 map 回溯构建完整链，跳过压缩节点。

        与 trace_chain 的区别：
        - trace_chain: 遇到 parent_uuid=None 就停止（活跃链）
        - trace_full_chain: 遇到 parent_uuid=None 时，检查 unzip_last_uuid
          - 如果 unzip_last_uuid 不为空：跳过当前节点（压缩节点），从 unzip_last_uuid 继续
          - 如果 unzip_last_uuid 为空：停止（根节点）

        返回 [根, ..., 叶] 顺序，不含压缩节点。
        """
        if not self._all_items:
            return []

        # 确定起点
        if from_uuid is not None:
            start_uuid = from_uuid
        else:
            leaf_candidates = [
                u for u in self._lines_order if u not in self._all_parent_uuids
            ]
            start_uuid = (
                leaf_candidates[-1] if leaf_candidates else self._lines_order[-1]
            )

        # 从起点倒序遍历到根
        chain: list[T] = []
        current_uuid = start_uuid
        while current_uuid:
            item = self._all_items.get(current_uuid)
            if not item:
                break

            # 压缩节点：parent_uuid=None, unzip_last_uuid is set → 跳过
            if item.parent_uuid is None and item.unzip_last_uuid is not None:
                current_uuid = item.unzip_last_uuid
                continue

            chain.append(item)

            if item.parent_uuid is None:
                break

            current_uuid = item.parent_uuid

        chain.reverse()
        return chain

    def walk_full_chain(self, from_uuid: str | None = None) -> list[T]:
        """从内存 map 回溯构建完整链，包含压缩节点。

        与 trace_full_chain 的区别：
        - trace_full_chain: 跳过压缩节点
        - walk_full_chain: 包含压缩节点（可通过 unzip_last_uuid is not None 识别）

        返回 [根, ..., 叶] 顺序，包含压缩节点。
        """
        if not self._all_items:
            return []

        # 确定起点
        if from_uuid is not None:
            start_uuid = from_uuid
        else:
            leaf_candidates = [
                u for u in self._lines_order if u not in self._all_parent_uuids
            ]
            start_uuid = (
                leaf_candidates[-1] if leaf_candidates else self._lines_order[-1]
            )

        # 从起点倒序遍历到根
        chain: list[T] = []
        current_uuid = start_uuid
        while current_uuid:
            item = self._all_items.get(current_uuid)
            if not item:
                break

            chain.append(item)

            if item.parent_uuid is None and item.unzip_last_uuid is not None:
                # 压缩节点：包含它，然后沿 unzip_last_uuid 继续
                current_uuid = item.unzip_last_uuid
            elif item.parent_uuid is None:
                break
            else:
                current_uuid = item.parent_uuid

        chain.reverse()
        return chain

    # ── 只读接口 ──────────────────────────────

    @overload
    def __getitem__(self, key: int) -> T: ...
    @overload
    def __getitem__(self, key: slice) -> list[T]: ...

    def __getitem__(self, key: int | slice) -> T | list[T]:
        return self._data[key]

    def __len__(self) -> int:
        return len(self._data)

    def __iter__(self) -> Iterator[T]:
        return iter(self._data)

    def __contains__(self, item: object) -> bool:
        return item in self._data

    def __repr__(self) -> str:
        return f"TrackedList({self._data!r})"
