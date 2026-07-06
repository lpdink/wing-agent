"""
TrackedList — 持久化泛型列表，UUID + parentUuid 链模型。

核心设计：
- history.jsonl：每行一条消息记录（uuid, parentUuid, ts + 消息完整字段）
- 消息以 uuid 为唯一标识，以 parentUuid 构建链拓扑
- 所有写操作都是 append-only（只追加，不修改已有行）
- newest.json：写，但不读 — 仅作为人类可读的活跃链快照
- 始终以类型化对象工作，无 raw dict 操作
- 内存中维护 full chain map（_all_items），find/trace_chain 从内存读取
- 仅 load, _write_message_line, _write_newest 与外存文件打交道

已知问题：
1. append 成功写入 JSONL 却在 newest.json 更新前崩溃，那条消息虽已安全落盘，
   但下次启动读 newest.json，找不到永远丢失。——但现在不读 newest.json 了，
   此问题已消除。（但是 newest.json 与 history.jsonl 不一致仍可能存在）
2. 并发不安全。
"""

from __future__ import annotations

import json
import os
import uuid as uuid_mod
from datetime import datetime
from pathlib import Path
from typing import Generic, Iterator, List, Type, TypeVar, Union, overload

from wing.schema import ChainNode

T = TypeVar("T", bound=ChainNode)


class TrackedList(Generic[T]):
    """持久化泛型列表。消息日志 + parentUuid 链。newest.json 只写不读。"""

    _NEWEST = "newest.json"
    _HISTORY = "history.jsonl"

    def __init__(self, path: Path | str | None = None) -> None:
        self._path: Path | None = Path(path) if path else None
        self._data: list[T] = []
        self._type: type[T] | None = None
        self._last_uuid: str | None = None  # 活跃链最后一条消息的 uuid

        # 内存 full chain map：仅 load / 写操作维护，find/trace_chain 从此读取
        self._all_items: dict[str, T] = {}  # uuid -> item
        self._all_parent_uuids: set[str] = set()
        self._lines_order: list[str] = []

    # ── 属性 ──────────────────────────────────

    @property
    def path(self) -> Path | None:
        """持久化目录（可能为 None，如果未初始化）。"""
        return self._path

    @property
    def active_chain(self) -> list[T]:
        """当前上下文窗口（_data 的副本）。"""
        return list(self._data)

    @property
    def last_uuid(self) -> str | None:
        """活跃链末尾消息的 uuid。"""
        return self._last_uuid

    # ── 内部：路径与类型 ──────────────────────

    def _ensure_type(self, items: list[T]) -> None:
        """首次修改时锁定类型，后续检查兼容性。同时填充 uuid/parentUuid。"""
        for it in items:
            if self._type is None:
                self._type = type(it)
                if self._path is None:
                    self._path = Path(f"./{self._type.__name__}/")
            elif not isinstance(it, self._type):
                raise TypeError(
                    f"expected {self._type.__name__}, got {type(it).__name__}"
                )
            # 填充 uuid 和 parentUuid
            if it.uuid is None:
                it.uuid = str(uuid_mod.uuid4())
            if it.parent_uuid is None:
                it.parent_uuid = self._last_uuid
            # 更新 _last_uuid，供下一条消息链接
            if it.uuid:
                self._last_uuid = it.uuid

    def _assert_initialized(self) -> Path:
        """Assert path and type are set. Ensure directory exists. Returns _path."""
        if not self._path or not self._type:
            raise RuntimeError("TrackedList not initialized — no path or type set")
        self._path.mkdir(parents=True, exist_ok=True)
        return self._path

    # ── 内部：内存 map 维护 ───────────────────

    def _update_memory(self, item: T) -> None:
        """将一条消息同步到内存 full chain map。"""
        if item.uuid:
            self._all_items[item.uuid] = item
            if item.parent_uuid:
                self._all_parent_uuids.add(item.parent_uuid)
            self._lines_order.append(item.uuid)

    def _load_all_items(self) -> None:
        """从 history.jsonl 读取所有消息到内存 map。"""
        self._all_items = {}
        self._all_parent_uuids = set()
        self._lines_order = []

        if not self._path or not self._type:
            return
        hist_path = self._path / self._HISTORY
        if not hist_path.exists():
            return

        with open(hist_path, "r", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    item = self._type.model_validate_json(line)
                except Exception:
                    continue
                self._update_memory(item)

    # ── 内部：持久化 ──────────────────────────

    def _write_newest(self) -> None:
        """原子写入 newest.json：tmp + fsync + rename。"""
        path = self._assert_initialized()
        snapshot = [m.model_dump() for m in self._data]
        target = path / self._NEWEST
        tmp = target.with_suffix(f".tmp.{os.getpid()}")
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(snapshot, f, ensure_ascii=False)
            f.flush()
            os.fsync(f.fileno())
        os.rename(tmp, target)

    def _write_message_line(self, item: T) -> None:
        """追加一条消息行到 history.jsonl：写入消息的完整 model_dump + ts。"""
        self._write_message_lines([item])

    def _write_message_lines(self, items: list[T]) -> None:
        """批量追加多条消息行到 history.jsonl（单次文件操作）。"""
        path = self._assert_initialized()
        hist_path = path / self._HISTORY
        lines: list[str] = []
        for item in items:
            entry = item.model_dump()
            entry["ts"] = datetime.now().isoformat()
            lines.append(json.dumps(entry, ensure_ascii=False))
        with open(hist_path, "a", encoding="utf-8") as f:
            f.write("\n".join(lines) + "\n")
            f.flush()
            os.fsync(f.fileno())

    # ── 静态恢复接口 ──────────────────────────

    @classmethod
    def load(cls, path: Path | str, item_type: Type[T]) -> TrackedList[T]:
        """从 history.jsonl 完全恢复状态（冷启动）。

        1. 将 history.jsonl 全量读入内存 map
        2. 从内存 map 重建活跃链（trace_chain）

        newest.json 不再被读取，仅作为人类可读的缓存文件写入。
        """
        tl = cls.__new__(cls)
        tl._path = Path(path)
        tl._data = []
        tl._type = item_type
        tl._last_uuid = None
        tl._all_items = {}
        tl._all_parent_uuids = set()
        tl._lines_order = []
        tl._path.mkdir(parents=True, exist_ok=True)

        # 1. 将 history.jsonl 全量读入内存
        tl._load_all_items()

        # 2. 从内存 map 重建活跃链
        tl._data = tl.trace_chain()

        # 从 _data 中恢复 _last_uuid
        for item in tl._data:
            if item.uuid:
                tl._last_uuid = item.uuid

        return tl

    # ── 写操作（全部 append-only）──────────────

    def append(self, item: T) -> None:
        """追加元素。填充 uuid/parentUuid，写入消息行，更新 newest.json。"""
        self._ensure_type([item])
        self._data.append(item)
        self._last_uuid = item.uuid
        self._write_message_line(item)
        self._update_memory(item)
        self._write_newest()

    def extend(self, items: Union[Iterator[T], List[T]]) -> None:
        """批量追加多条消息。每条填充 uuid/parentUuid，写入消息行，更新 newest.json。"""
        lst = list(items)
        self._ensure_type(lst)
        self._data.extend(lst)
        for item in lst:
            self._update_memory(item)
            self._last_uuid = item.uuid
        self._write_message_lines(lst)
        self._write_newest()

    def extend_detached(self, items: Union[Iterator[T], List[T]]) -> None:
        """批量追加多条消息，不自动填充 uuid/parentUuid。

        类似 append_detached 的批量版本。调用方已自行设置好拓扑关系。
        一次性写入 JSONL（单次文件操作），更新 _data 和 newest.json。
        """
        lst = list(items)
        if not lst:
            return

        for item in lst:
            if self._type is None:
                self._type = type(item)
                if self._path is None:
                    self._path = Path(f"./{self._type.__name__}/")
            elif not isinstance(item, self._type):
                raise TypeError(
                    f"expected {self._type.__name__}, got {type(item).__name__}"
                )

        self._assert_initialized()
        self._data.extend(lst)
        for item in lst:
            self._update_memory(item)
            self._last_uuid = item.uuid
        self._write_message_lines(lst)
        self._write_newest()

    def append_detached(self, item: T) -> None:
        """追加消息，但不自动填充 uuid/parentUuid。

        调用方已自行设置好拓扑关系时使用（compact/rewind 场景）。
        仍会写 JSONL、更新 _data 和 _last_uuid、更新 newest.json。
        """
        if self._type is None:
            self._type = type(item)
        elif not isinstance(item, self._type):
            raise TypeError(
                f"expected {self._type.__name__}, got {type(item).__name__}"
            )
        if self._path is None:
            self._path = Path(f"./{self._type.__name__}/")
        self._assert_initialized()
        self._data.append(item)
        self._last_uuid = item.uuid
        self._write_message_line(item)
        self._update_memory(item)
        self._write_newest()

    def set_tip(self, uuid: str) -> None:
        """将活跃链末尾切换到指定 uuid。

        1. 从内存 map 重建以 uuid 为叶的链（沿 parent_uuid 回溯到根）
        2. 用重建结果替换 _data
        3. 更新 _last_uuid = uuid
        4. 更新 newest.json
        """
        self._assert_initialized()
        chain = self.trace_chain(from_uuid=uuid)
        if not chain and uuid is not None:
            raise ValueError(f"uuid {uuid} not found in history")
        self._data = chain
        self._last_uuid = uuid
        self._write_newest()

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
