"""会话标签的打标时间（``tag_meta``）场景：随增删维护、不水合、老数据降级。

被守的语义（``store`` + ``session.tags`` + gateway 路由）：

- 标签**实际加入**时记录 ``added_at``（创建即打标同样记录）；tag 端点 /
  ``/api/session/list`` / ``/api/session/info`` 三面一致，并与磁盘
  ``metadata.json`` 独立对账；
- 幂等 no-op（重复 add 已存在的标签）**不刷新**时间——"后加的排前面"这类
  前端语义要的是真正的新加入；
- 移除标签即删除记录；清空后 ``tag_meta`` 与 ``tags`` 一起从 metadata.json 消失；
- 逐出（inactive）会话打标：记录落盘且**不水合**（身份不被"弄醒"）；
- 老数据 / 手改（只有 tags、无记录）：投影为空记录，标签本身照常可用——
  时间未知不是标签失效。
"""

from __future__ import annotations

import json
from datetime import datetime

import pytest

from wing_probe import Probe, Session, Turn
from wing_probe.history import read_metadata

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
TAG_MODEL = "probe/tag-times"

HELLO = "hello tag times"


async def _create(probe: Probe, *, tags: list[str] | None = None) -> Session:
    driver = probe.driver_required
    body: dict = {
        "workspace": str(probe.workspace),
        "agent": {"model_id": TAG_MODEL, "yolo": True},
    }
    if tags is not None:
        body["tags"] = tags
    payload = await driver.http.request("POST", "/api/session/create", body=body)
    return await driver.attach(
        payload["session_id"], response=payload, workspace=probe.workspace
    )


async def _tag(probe: Probe, session_id: str, **ops: list[str]) -> dict:
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
    driver = probe.driver_required
    await driver.http.unsubscribe(session_id, driver.client_id)
    payload = await driver.http.request(
        "POST", "/api/session/release", body={"session_id": session_id}
    )
    assert payload == {"ok": True, "released": True, "detail": "released"}, payload


def _assert_recorded(entry: dict, tag: str) -> str:
    """断言某标签有可解析的加入时间，返回该时间（本地 naive ISO）。"""
    meta = entry.get("tag_meta") or {}
    assert tag in meta, entry
    stamp = meta[tag].get("added_at")
    assert isinstance(stamp, str) and stamp, entry
    datetime.fromisoformat(stamp)  # 可解析即契约
    return stamp


@pytest.mark.probe_env(models=[TAG_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_tag_times_recorded_idempotent_and_dropped(probe: Probe) -> None:
    """打标时间随标签增删维护，且幂等 no-op 不刷新（三面 + 磁盘对账）。

    WHEN 创建带 ["pin"] 的会话，随后 add "executor"、重复同一 add、再
    remove "pin" 直至清空
    THEN 每个实际加入的标签有可解析的 added_at；重复 add 保持原时间；
    移除即删记录；清空后 tags / tag_meta 一起从磁盘消失。
    """
    probe.register(TAG_MODEL, Turn.of(text="ok"))
    session = await _create(probe, tags=["pin"])
    sid = session.session_id
    await session.chat(HELLO)

    # 创建即打标：时间三面一致（tag 端点 / list / info），并与磁盘对账。
    read = await _tag(probe, sid)
    created_at = _assert_recorded(read, "pin")
    assert _assert_recorded(await _list_entry(probe, sid), "pin") == created_at
    assert _assert_recorded(await session.info(), "pin") == created_at
    disk = read_metadata(probe.env.session_dir(sid))
    assert disk is not None and disk["tag_meta"]["pin"]["added_at"] == created_at

    # 新增标签记时间，老标签的原时间不受影响。
    added = await _tag(probe, sid, add=["executor"])
    fresh = _assert_recorded(added, "executor")
    assert _assert_recorded(added, "pin") == created_at, added

    # 幂等 no-op：重复 add 不刷新时间。
    repeat = await _tag(probe, sid, add=["executor"])
    assert repeat["added"] == [] and repeat["removed"] == [], repeat
    assert _assert_recorded(repeat, "executor") == fresh, repeat

    # 移除即删记录（其余记录保留）。
    removed = await _tag(probe, sid, remove=["pin"])
    assert removed["tags"] == ["executor"], removed
    assert "pin" not in (removed.get("tag_meta") or {}), removed
    assert _assert_recorded(removed, "executor") == fresh, removed
    disk = read_metadata(probe.env.session_dir(sid))
    assert disk is not None and "pin" not in (disk.get("tag_meta") or {}), disk

    # 清空：tags 与 tag_meta 一起从磁盘消失（零残留）。
    cleared = await _tag(probe, sid, remove=["executor"])
    assert cleared["tags"] == [] and cleared["tag_meta"] == {}, cleared
    disk = read_metadata(probe.env.session_dir(sid))
    assert disk is not None, disk
    assert "tags" not in disk and "tag_meta" not in disk, disk


@pytest.mark.probe_env(models=[TAG_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_evicted_tagging_records_time_without_hydration(probe: Probe) -> None:
    """逐出会话打标：时间走 store 直写路径落盘，会话保持 inactive。

    WHEN 会话发言后被逐出，在 inactive 状态下 add "pin"
    THEN 记录写入磁盘 metadata.json、list / tag 端点可读，身份全程 inactive；
    resume 后 info 携带同一条记录（内存与磁盘同源）。
    """
    probe.register(TAG_MODEL, Turn.of(text="ok"))
    session = await _create(probe)
    sid = session.session_id
    await session.chat(HELLO)

    await _evict(probe, sid)
    assert (await _list_entry(probe, sid))["status"] == "inactive"

    tagged = await _tag(probe, sid, add=["pin"])
    stamp = _assert_recorded(tagged, "pin")

    entry = await _list_entry(probe, sid)
    assert entry["status"] == "inactive", entry  # 不水合
    assert _assert_recorded(entry, "pin") == stamp
    disk = read_metadata(probe.env.session_dir(sid))
    assert disk is not None and disk["tag_meta"]["pin"]["added_at"] == stamp, disk

    resumed = await probe.resume(sid)
    info = await resumed.info()
    assert _assert_recorded(info, "pin") == stamp, info


@pytest.mark.probe_env(models=[TAG_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_legacy_metadata_without_records_projects_empty(probe: Probe) -> None:
    """老数据（只有 tags、无 tag_meta）：投影为空记录，标签照常可用并自愈。

    WHEN 手写一份"老版本" metadata.json（tags-only）给逐出会话
    THEN list / tag 端点读到标签本身、记录为空；再 add 新标签时写出的是
    清洗后的集合——老标签没有时间、新标签有（不凭空补时间）。
    """
    probe.register(TAG_MODEL, Turn.of(text="ok"))
    session = await _create(probe)
    sid = session.session_id
    await session.chat(HELLO)
    await _evict(probe, sid)

    # 模拟老版本 / 手改数据：有标签，没有任何记录。
    meta_path = probe.env.session_dir(sid) / "metadata.json"
    raw = json.loads(meta_path.read_text(encoding="utf-8"))
    raw["tags"] = ["favorite"]
    raw.pop("tag_meta", None)
    meta_path.write_text(json.dumps(raw), encoding="utf-8")

    read = await _tag(probe, sid)
    assert read["tags"] == ["favorite"] and read["tag_meta"] == {}, read
    entry = await _list_entry(probe, sid)
    assert entry["tags"] == ["favorite"] and entry["tag_meta"] == {}, entry

    # 之后的一次真实变更：新标签有时间，老标签保持"时间未知"。
    added = await _tag(probe, sid, add=["pin"])
    assert added["tags"] == ["favorite", "pin"], added
    assert set(added["tag_meta"]) == {"pin"}, added
    _assert_recorded(added, "pin")
