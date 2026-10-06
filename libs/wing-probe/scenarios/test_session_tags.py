"""会话标签（tags）场景：L0 标记闭环（原子 / 不水合 / fork 不继承）。

被守的语义（``store`` + ``session.manager`` + gateway 路由）：

- 创建可带标；``POST /api/session/tag`` 的 add / remove 单请求**原子**应用、
  幂等（重复添加 / 移除不存在都不产生新增删）；
- 读写**不水合**：逐出（inactive）的会话可读可打标，身份不被"弄醒"——
  打标后 ``/api/session/list`` 里仍是 ``inactive``；
- ``/api/session/list`` 与 ``/api/session/info`` 都携带 tags；
- fork 不继承源会话标签；
- 非法标签 400、未知会话 404；移除干净后 metadata.json 不带 ``tags`` 字段。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, Session, Turn
from wing_probe.driver import DriverHttpError
from wing_probe.history import read_metadata

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
TAG_MODEL = "probe/session-tags"

#: 让会话"有标题素材"的消息（列表只收有名字的会话——既有语义）。
HELLO = "hello tags"


async def _create(probe: Probe, *, tags: list[str] | None = None) -> Session:
    """直接打创建端点（便捷工厂没有 tags 参数）：返回已挂进路由表的句柄。"""
    driver = probe.driver_required
    body: dict = {
        "workspace": str(probe.workspace),
        "agent": {"model": TAG_MODEL, "yolo": True},
    }
    if tags is not None:
        body["tags"] = tags
    payload = await driver.http.request("POST", "/api/session/create", body=body)
    return await driver.attach(
        payload["session_id"], response=payload, workspace=probe.workspace
    )


async def _tag(probe: Probe, session_id: str, **ops: list[str]) -> dict:
    """调用标签端点（无 ops = 纯读）。"""
    driver = probe.driver_required
    return await driver.http.request(
        "POST", "/api/session/tag", body={"session_id": session_id, **ops}
    )


async def _list_entry(probe: Probe, session_id: str) -> dict:
    driver = probe.driver_required
    payload = await driver.http.request("GET", "/api/session/list")
    entries = {e["id"]: e for e in payload.get("sessions", [])}
    assert session_id in entries, sorted(entries)
    return entries[session_id]


async def _evict(probe: Probe, session_id: str) -> None:
    """逐出：断开订阅 → release（与 test_session_persistence 同款两步）。"""
    driver = probe.driver_required
    await driver.http.unsubscribe(session_id, driver.client_id)
    payload = await driver.http.request(
        "POST", "/api/session/release", body={"session_id": session_id}
    )
    assert payload == {"ok": True, "released": True, "detail": "released"}, payload


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_create_with_tags_atomic_mutation_and_idempotence(probe: Probe) -> None:
    """创建即打标；单请求 add+remove 原子应用；重复调用幂等；错误面 400/404。

    WHEN 创建带 ["seed", "task=wing-tags"] 的会话，其后一次请求同时
    ``add=["favorite","scheduler"] remove=["seed"]``、再重复一次
    THEN 读取（标签端点 / list / info 三面）与变更结果一致且顺序稳定；
    重复调用 added/removed 皆为 0（幂等）；非法标签 400、未知会话 404。
    """
    probe.register(TAG_MODEL, Turn.of(text="ok"))
    session = await _create(probe, tags=["seed", "task=wing-tags"])
    sid = session.session_id
    await session.chat(HELLO)

    # 创建即打标：三面读取一致（标签端点 / list / info）。
    read = await _tag(probe, sid)
    assert read["tags"] == ["seed", "task=wing-tags"], read
    assert read["added"] == [] and read["removed"] == [], read
    assert (await _list_entry(probe, sid))["tags"] == ["seed", "task=wing-tags"]
    assert (await session.info())["tags"] == ["seed", "task=wing-tags"]

    # 单请求原子增删：remove 的删掉、add 的追加在后、旧序保持。
    mutation = await _tag(probe, sid, add=["favorite", "scheduler"], remove=["seed"])
    assert mutation["tags"] == ["task=wing-tags", "favorite", "scheduler"], mutation
    assert mutation["added"] == ["favorite", "scheduler"], mutation
    assert mutation["removed"] == ["seed"], mutation

    # 幂等：重复同一请求不产生任何新增删。
    repeat = await _tag(probe, sid, add=["favorite", "scheduler"], remove=["seed"])
    assert repeat["tags"] == mutation["tags"], repeat
    assert repeat["added"] == [] and repeat["removed"] == [], repeat

    # 错误面：非法标签 400（点名违规值）、未知会话 404。
    with pytest.raises(DriverHttpError) as bad:
        await _tag(probe, sid, add=["bad tag"])
    assert bad.value.status == 400, bad.value.call.render()
    assert "bad tag" in str(bad.value.call.response), bad.value.call.response
    with pytest.raises(DriverHttpError) as missing:
        await _tag(probe, "20250101-000000-deadbeef", add=["x"])
    assert missing.value.status == 404, missing.value.call.render()


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_evicted_session_can_be_tagged_without_waking(probe: Probe) -> None:
    """逐出会话可读可打标，且保持 inactive（不水合）；移除干净后不落 tags 字段。

    WHEN 会话发言后被逐出（inactive），在 inactive 状态下 add "favorite"、
    再 remove 掉并重读
    THEN 标签读写全程不把会话换入内存（list 里始终 inactive）、
    磁盘 metadata.json 只有真实存在的标签；顺序与幂等语义成立。
    """
    probe.register(TAG_MODEL, Turn.of(text="ok"))
    session = await _create(probe)
    sid = session.session_id
    await session.chat(HELLO)

    await _evict(probe, sid)
    entry = await _list_entry(probe, sid)
    assert entry["status"] == "inactive", entry
    assert entry["tags"] == [], entry

    # 逐出状态下打标：成功，且**不水合**（list 里仍是 inactive）。
    added = await _tag(probe, sid, add=["favorite"])
    assert added["tags"] == ["favorite"], added
    assert added["added"] == ["favorite"], added
    entry = await _list_entry(probe, sid)
    assert entry["status"] == "inactive", entry
    assert entry["tags"] == ["favorite"], entry

    # 磁盘事实（probe 独立解析，不依赖网关自述）。
    meta = read_metadata(probe.env.session_dir(sid))
    assert meta is not None and meta.get("tags") == ["favorite"], meta

    # 移除干净 → tags 字段从 metadata.json 消失（空列表不落盘）。
    removed = await _tag(probe, sid, remove=["favorite"])
    assert removed["tags"] == [] and removed["removed"] == ["favorite"], removed
    meta = read_metadata(probe.env.session_dir(sid))
    assert meta is not None and "tags" not in meta, meta
    assert (await _list_entry(probe, sid))["status"] == "inactive"

    # 水合回来标签语义不变（此时才把会话换入内存）。
    resumed = await probe.resume(sid)
    assert (await resumed.info())["tags"] == []


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_fork_does_not_inherit_tags(probe: Probe) -> None:
    """fork 不继承标签：子会话从零开始，源会话标签原样保留。"""
    probe.register(TAG_MODEL, Turn.of(text="ok"))
    session = await _create(probe, tags=["favorite", "task=wing-tags"])
    sid = session.session_id
    await session.chat(HELLO)

    child = await session.fork("current")

    assert (await _tag(probe, child.session_id))["tags"] == []
    assert (await _tag(probe, sid))["tags"] == ["favorite", "task=wing-tags"]


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_tagged_session_without_messages_is_listed_immediately(
    probe: Probe,
) -> None:
    """创建即打标：首条消息落盘前 list 就能找到（"创建即打标"的窗口期）。

    WHEN 创建一个带 ["wing-probe"] 的会话、不发任何消息
    THEN /api/session/list 立即包含它（tags 已带、name 为空）——"按标签找回"
    从会话出生那一刻成立，而不是等首条消息。
    """
    session = await _create(probe, tags=["wing-probe"])

    entry = await _list_entry(probe, session.session_id)
    assert entry["tags"] == ["wing-probe"], entry
    assert not entry.get("name"), entry
