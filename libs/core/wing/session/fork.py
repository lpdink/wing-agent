# wing/session/fork.py
"""fork 的数据搬迁：记录前缀切片、链拓扑重映射、媒体对象复制与物化。

全部是无 ``self`` 的纯函数（外加两个经 ``SessionStore`` 的 best-effort 复制），
与 ``SessionManager`` 的生命周期 / 逐出 / 归档簿记无关。共同前提：fork 与加载
都是**读路径**——任何一条坏记录都不该炸掉整单（降级或跳过，见各函数说明）。

``session.manager`` 转发 import 前四个（它自己调用 `_fork_slice` /
`_remap_record_uuids` / `_materialize_loaded_media` / `_copy_session_media`），
因此这四个名字在 ``wing.session.manager`` 命名空间里仍可达。
"""

from __future__ import annotations

from collections.abc import Iterable
from uuid import uuid4

from wing.common.logger import log
from wing.schema import ChainNode, Message
from wing.store import SessionStore, canonical_record


def _fork_slice(
    records: Iterable[dict],
    target_uuid: str,
) -> tuple[list[dict], str | None]:
    """切出 fork 的记录前缀（**append 顺序**）与 draft。

    - ``"current"`` → 全部记录，draft 为空串；
    - 其他 → 目标记录**之前**的全部记录（目标自身不进拷贝：它由响应里的
      draft 重新发送），draft = 目标记录的 content。

    draft 走 ``Message`` 的访问器（assistant 的 content 由 ``content_blocks``
    派生——记录里**不存**副本），因此与"用户看到的那条消息"同源：目标可以是
    压缩节点（``[Compact] …``）或任何带文本的 assistant 记录。

    用记录口径（而不是链遍历）是刻意的：被压缩区间的记录原样保留在子会话里
    （用户仍可回退 / 分叉到压缩前的 User Message），而"活跃链从哪里开始"由
    压缩节点自身编码（``parent_uuid=None`` + ``unzip_last_uuid``）——子会话
    加载时沿 parent_uuid 回溯到该节点即止，已摘要内容不会复活。选压缩前的
    节点时，压缩节点是后来追加的记录，切在前缀之外 → 子会话里压缩仿佛没发生
    过（等价于"在压缩点之前分叉"）。

    ``records`` 是**迭代器**：命中目标即停，fork 点之后的记录不读也不解析
    ——只有 ``"current"`` 需要走到末尾（全部记录都是前缀）。

    Raises:
        ValueError: 目标 uuid 不在记录里，或指向的是事件记录（事件不是对话节点）。
    """
    if target_uuid == "current":
        return list(records), ""
    prefix: list[dict] = []
    for record in records:
        if record.get("uuid") == target_uuid:
            if record.get("role") == "event":
                raise ValueError(
                    f"uuid {target_uuid!r} is an event record, not a Message"
                )
            return prefix, _record_draft(record)
        prefix.append(record)
    raise ValueError(f"uuid {target_uuid!r} not found in {len(prefix)} record(s)")


def _record_draft(record: dict) -> str:
    """目标记录的 draft 文本：走 ``Message`` 派生（assistant 的 content 在
    ``content_blocks`` 里，记录本身不带副本）；脏记录按"无文本"降级。

    fork 是**读请求**，不能因为一条记录校验不过就整单失败——降级为空串与
    "目标没有可发送文本"同义。``content`` 不做类型校验（私有属性直通），
    因此非字符串值也在这里折成空串。
    """
    try:
        draft = Message.model_validate(record).content
    except Exception:
        return ""
    return draft if isinstance(draft, str) else ""


def _remap_record_uuids(records: list[dict]) -> list[dict]:
    """复制记录（顺带规范化）并把链拓扑 uuid 全量重映射到子会话的 uuid 空间。

    重映射 ``uuid`` / ``parent_uuid`` / ``unzip_last_uuid`` / ``usage_boundary_uuid``
    四个键。前缀口径下前者们的引用都指向前缀内部（parent 必然更早创建、unzip 指向
    区间末），因此重映射后子记录集自洽：不留悬空引用，也不需要清空任何引用——压缩
    节点的 unzip 指向重映射后的区间末记录，子会话的 fork 候选列表与源会话一致。

    ``usage_boundary_uuid`` 是唯一可能指向前缀之外的键：fork 点落在压缩后被保留的
    tail 中间时，边界（换入时刻的链尖）在后头。删键即缺席——读取侧把"没有边界"当作
    历史口径（信任保留区 usage），方向与压缩前一致，不损坏数据。

    缺省是防御：万一出现前缀外的引用，宁可让它成为根节点（删键 = 缺席即默认），
    也不留跨 session 的引用。

    ``canonical_record`` 让**复制也走落盘规范形**：fork 直接复用源记录的原始
    dict（不经 ``TrackedList._to_record``），不过这一道，存量旧形态的冗余字段
    （派生扁平字段 / null）就会被逐字节搬进子会话，越 fork 越多。

    只做**顶层浅拷贝**：重写的三个键都在顶层，而记录（含嵌套值）对读取方
    一律只读（``MessageLog`` 的记录契约：file 后端每次重新解析，memory
    后端直接交出内部结构，两边都没有就地改写记录的路径），深拷贝没有额外
    保护面。
    """
    clones = [canonical_record(record) for record in records]
    uuid_map: dict[str, str] = {}
    for clone in clones:
        uuid = clone.get("uuid")
        if isinstance(uuid, str):
            uuid_map[uuid] = str(uuid4())
    for clone in clones:
        for key in (
            "uuid",
            "parent_uuid",
            "unzip_last_uuid",
            "usage_boundary_uuid",
        ):
            value = clone.get(key)
            if not isinstance(value, str):
                continue
            mapped = uuid_map.get(value)
            if mapped is None:
                # 前缀外引用：**删键**而不是写 None（缺席即默认，与 None 对
                # 加载等价——记录重新成为根节点，且不把 null 带进子会话）。
                clone.pop(key, None)
            else:
                clone[key] = mapped
    return clones


def _message_media_ids(nodes: Iterable[ChainNode]) -> list[str]:
    """链节点引用的媒体 id（去重、保持出现序）。

    ``ChainNode`` 家族里只有 ``Message`` 带 ``media``（事件没有）——按属性
    探测，加载路径对两类记录混排的链无感。
    """
    seen: dict[str, None] = {}
    for node in nodes:
        for ref in getattr(node, "media", None) or []:
            seen.setdefault(ref.id, None)
    return list(seen)


def _record_media_ids(records: Iterable[dict]) -> list[str]:
    """raw 记录引用的媒体 id（去重、保持出现序；坏形态跳过）。

    记录契约归 ``MessageLog`` 管：这里只做防御性取值（media 不是数组、
    元素不是 dict、id 不是 str 一律跳过）——fork 复制是 best-effort 路径，
    不值得为一条坏记录炸掉整个 fork。
    """
    seen: dict[str, None] = {}
    for record in records:
        refs = record.get("media")
        if not isinstance(refs, list):
            continue
        for ref in refs:
            if isinstance(ref, dict) and isinstance(ref.get("id"), str):
                seen.setdefault(ref["id"], None)
    return list(seen)


def _materialize_loaded_media(
    store: SessionStore, session_id: str, nodes: Iterable[ChainNode]
) -> None:
    """加载路径：把活跃链引用的媒体对象幂等物化进会话目录（best-effort）。

    老会话的对象在遗留共享池（``<root>/.media/``）里，读路径虽能双读兜底，
    但"会话自包含"要求对象在**首次加载**时就搬进 ``<sid>/media/``——此后
    整个会话目录拷贝到另一台机器即可用。对象通常已在目录里（快路径 stat
    即返回），miss 时才碰老池。失败只 WARN（含历史里的非法 id）：读取侧
    双读仍在，最坏是该位降级为占位文本，绝不阻断加载。
    """
    for media_id in _message_media_ids(nodes):
        try:
            store.materialize(session_id, media_id)
        except Exception as e:  # noqa: BLE001 —— 迁移失败不能阻断会话加载
            log.warning(
                f"Session {session_id}: materialize media '{media_id[:8]}' "
                f"failed: {e} (read path keeps its fallback)"
            )


def _copy_session_media(
    store: SessionStore, source_id: str, child_id: str, records: Iterable[dict]
) -> None:
    """fork：把子会话记录引用的媒体对象复制进子会话目录（自包含；best-effort）。

    复制源是**源会话目录优先、老池回退**（经 ``read_media``，顺带把老池命中
    物化进源目录）。失败只 WARN 不阻断 fork——读取侧双读兜底仍在，代价是复制
    失败的那个对象在子会话里会降级为占位（回退池不含源会话目录，这是刻意的：
    不给"自包含"留第二条隐式依赖）。判据是常规路径全部复制成功（probe 钉住）。
    """
    for media_id in _record_media_ids(records):
        try:
            data = store.read_media(source_id, media_id)
            if data is None:
                log.warning(
                    f"fork: media '{media_id[:8]}' referenced by session "
                    f"'{source_id}' not found (session dir or legacy pool); "
                    f"child '{child_id}' will degrade it to a placeholder"
                )
                continue
            store.write_media(child_id, media_id, data)
        except Exception as e:  # noqa: BLE001 —— 复制失败不能阻断 fork
            log.warning(
                f"fork: copying media '{media_id[:8]}' from '{source_id}' to "
                f"'{child_id}' failed: {e}"
            )
