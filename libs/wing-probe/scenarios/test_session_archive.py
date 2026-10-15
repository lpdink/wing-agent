"""会话归档（archive）红线场景：往返纯移动 / 冻结 / 幂等 / sweep 与豁免。

被守的语义（store + session.manager + gateway 路由 + BackgroundScheduler）：

- **纯移动**：归档 → 恢复往返后链逐记录一致、history 逐字节一致（红线）；
  归档后的会话从 ``/api/session/list`` 消失，只出现在归档列表（读索引）；
- **冻结**：归档区不参与解析——resume / send / create-or-adopt 按其 id 的调用
  一律 404 且带结构化 ``error="session_archived"``（与"真不存在"的
  ``not_found`` 区分）；
- **幂等**：重复归档 / 恢复 200 + ``changed=false``；
- **钉住**：订阅中的会话 409；memory 后端 409（不支持归档）；
- **sweep**：超过 ``sessions.archive.idle_days`` 的会话自动归档；带 ``pin``
  标签的豁免；``idle_days <= 0`` = 关闭；内存中的过期会话先逐出再移动
  （live 目录不得复活——数据分裂红线）；
- **保留命名空间**：``.archive`` 不成会话、不进列表。

环境：``sessions.archive.idle_days = 0`` 关闭自动 sweep（显式归档不受影响）；
sweep 场景用「秒级扫描 + 分钟级阈值」（种子时间戳拨到 2020，新建会话远在
阈值内——阈值不能再小，否则刚建的会话也会老化，断言失去区分度）。
"""

from __future__ import annotations

import asyncio
import json
import time
from pathlib import Path

import pytest

from wing_probe import Probe, Turn
from wing_probe.driver import DriverHttpError
from wing_probe.driver.session import Driver

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
ROUNDTRIP_MODEL = "probe/archive-roundtrip"
CONFLICT_MODEL = "probe/archive-conflict"
SWEEP_MODEL = "probe/archive-sweep"
DISABLED_MODEL = "probe/archive-disabled"

#: 显式归档场景：关掉自动 sweep（它的存在只会在恢复后抢跑，干扰断言）。
NO_SWEEP: dict = {"archive": {"idle_days": 0}}

#: sweep 场景：0.002 天 ≈ 2.9 分钟——种子（2020）远超阈值，新建会话远在阈值内。
SWEEP_CONFIG: dict = {
    "archive": {
        "idle_days": 0.002,
        "sweep_interval_seconds": 0.5,
        "startup_delay_seconds": 0.5,
    }
}

#: 关闭场景（用户语义：<= 0 = 关）。
DISABLED_CONFIG: dict = {
    "archive": {
        "idle_days": -1,
        "sweep_interval_seconds": 0.5,
        "startup_delay_seconds": 0.5,
    }
}

OLD_TIMESTAMP = "2020-01-01T00:00:00"

#: 状态翻转的轮询预算（扫描 0.5s，留足抖动余量）。
POLL_DEADLINE = 30.0
POLL_INTERVAL = 0.2


def _driver(probe: Probe) -> Driver:
    return probe.driver_required


async def _archive(probe: Probe, session_id: str, *, archived: bool) -> dict:
    """``POST /api/session/archive``（非 2xx 由 DriverHttpError 带出）。"""
    return await _driver(probe).http.request(
        "POST",
        "/api/session/archive",
        body={"session_id": session_id, "archived": archived},
    )


async def _archived_entries(probe: Probe) -> list[dict]:
    payload = await _driver(probe).http.request("GET", "/api/session/archive")
    return payload.get("sessions", [])


async def _live_ids(probe: Probe) -> set[str]:
    payload = await _driver(probe).http.request("GET", "/api/session/list")
    return {entry["id"] for entry in payload.get("sessions", [])}


async def _wait_archived(probe: Probe, session_id: str, *, expected: bool) -> bool:
    """轮询归档列表直到该 id 的归属等于 ``expected``（超时即失败）。"""
    deadline = time.monotonic() + POLL_DEADLINE
    while True:
        ids = {entry["id"] for entry in await _archived_entries(probe)}
        if (session_id in ids) == expected:
            return expected
        if time.monotonic() > deadline:
            raise AssertionError(
                f"session {session_id} archived={session_id in ids} stayed put for "
                f"{POLL_DEADLINE:.0f}s, expected archived={expected}"
            )
        await asyncio.sleep(POLL_INTERVAL)


def _seed_stale_session(
    probe: Probe, session_id: str, *, tags: list[str] | None = None
) -> Path:
    """直接落盘一个"很久以前"的会话（不经网关）——sweep 的输入素材。"""
    session_dir = probe.env.session_dir(session_id)
    session_dir.mkdir(parents=True, exist_ok=True)
    metadata: dict = {
        "session_name": f"old-{session_id}",
        "last_interaction": OLD_TIMESTAMP,
    }
    if tags:
        metadata["tags"] = tags
    (session_dir / "metadata.json").write_text(json.dumps(metadata), encoding="utf-8")
    (session_dir / "history.jsonl").write_text(
        json.dumps({"role": "user", "content": "old", "uuid": f"u-{session_id}"})
        + "\n",
        encoding="utf-8",
    )
    return session_dir


def _rewrite_last_interaction(probe: Probe, session_id: str) -> None:
    """把一个内存会话的落盘时间戳拨旧（sweep 的终检读磁盘）。"""
    session_dir = probe.env.session_dir(session_id)
    metadata = json.loads((session_dir / "metadata.json").read_text(encoding="utf-8"))
    metadata["last_interaction"] = OLD_TIMESTAMP
    (session_dir / "metadata.json").write_text(json.dumps(metadata), encoding="utf-8")


@pytest.mark.probe_env(models=[ROUNDTRIP_MODEL], sessions=NO_SWEEP)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_archive_roundtrip_is_a_pure_move_and_freezes(probe: Probe) -> None:
    """归档 → 冻结 → 恢复：链与字节逐字一致（红线），列表归属翻转。"""
    probe.register(
        ROUNDTRIP_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text="reply two"),
        Turn.of(text="reply three"),
    )
    session = await probe.session(model=ROUNDTRIP_MODEL)
    await session.chat("hello")
    await session.chat("again")
    session_id = session.session_id

    before = probe.history(session)
    records_before = list(before.records)
    live_dir = probe.env.session_dir(session_id)
    live_bytes = (live_dir / "history.jsonl").read_bytes()

    # 订阅中的会话被钉住（先退订；退订后归档走"先逐出再移动"路径）。
    with pytest.raises(DriverHttpError) as excinfo:
        await _archive(probe, session_id, archived=True)
    assert excinfo.value.status == 409
    assert "subscribed" in excinfo.value.call.response.get("detail", "")
    await _driver(probe).http.unsubscribe(session_id, _driver(probe).client_id)

    archived = await _archive(probe, session_id, archived=True)
    assert archived == {
        "ok": True,
        "session_id": session_id,
        "archived": True,
        "changed": True,
        "detail": "archived",
    }

    # 纯移动：live 目录消失、归档目录字节不变。
    assert not live_dir.exists()
    archived_dir = probe.env.sessions_path / ".archive" / session_id
    assert (archived_dir / "history.jsonl").read_bytes() == live_bytes

    # 列表归属：live 消失、归档列表可见（标题 / 模型 / 归档时间）。
    assert session_id not in await _live_ids(probe)
    entries = await _archived_entries(probe)
    assert [entry["id"] for entry in entries] == [session_id]
    assert entries[0]["name"] == "hello"
    assert entries[0]["archived_at"]
    assert entries[0]["model_id"] == ROUNDTRIP_MODEL
    assert ".archive" not in await _live_ids(probe)  # 保留命名空间不成会话

    # 冻结：resume / send 一律 404 + 结构化标记（与"真不存在"区分）。
    for path, body in (
        ("/api/session/resume", {"session_id": session_id}),
        ("/api/session/send", {"session_id": session_id, "content": "knock"}),
    ):
        with pytest.raises(DriverHttpError) as excinfo:
            await _driver(probe).http.request("POST", path, body=body)
        assert excinfo.value.status == 404, path
        assert excinfo.value.call.response.get("error") == "session_archived", path
        assert "restore" in excinfo.value.call.response.get("detail", ""), path

    # 幂等：重复归档不报错、不移动。
    again = await _archive(probe, session_id, archived=True)
    assert again["changed"] is False and again["detail"] == "already archived"

    # 恢复：字节一致 + 继续对话后链的前缀逐记录一致（红线）。
    restored = await _archive(probe, session_id, archived=False)
    assert restored["changed"] is True and restored["detail"] == "restored"
    assert (live_dir / "history.jsonl").read_bytes() == live_bytes

    # 重新订阅要走显式调用（句柄已挂载，``resume`` 不会二次订阅）。
    await _driver(probe).http.subscribe(session_id, _driver(probe).client_id)
    resumed = _driver(probe).get(session_id)
    await resumed.chat("after restore")
    after = list(probe.history(resumed).records)
    assert len(after) > len(records_before)
    assert after[: len(records_before)] == records_before, (
        "归档往返改变了历史记录（红线：纯移动不得改任何字节）"
    )

    # 恢复幂等 + 未知 id 是普通 not_found（不冒充归档）。
    not_archived = await _archive(probe, session_id, archived=False)
    assert not_archived["changed"] is False
    assert not_archived["detail"] == "not archived"
    with pytest.raises(DriverHttpError) as excinfo:
        await _archive(probe, "no-such-session-id", archived=True)
    assert excinfo.value.status == 404
    assert excinfo.value.call.response.get("error") == "not_found"


@pytest.mark.probe_env(models=[CONFLICT_MODEL], sessions=NO_SWEEP)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_archive_conflicts_memory_backend_and_create_or_adopt(
    probe: Probe,
) -> None:
    """归档的冲突面：memory 后端不支持；create-or-adopt 撞归档 id 被拒。"""
    probe.register(CONFLICT_MODEL, Turn.of(text="reply"))
    session = await probe.session(model=CONFLICT_MODEL)
    await session.chat("hi")
    session_id = session.session_id
    driver = _driver(probe)
    await driver.http.unsubscribe(session_id, driver.client_id)
    assert (await _archive(probe, session_id, archived=True))["changed"] is True

    # create-or-adopt 撞归档 id：报"已归档"，绝不新建同 id 的 live 会话。
    with pytest.raises(DriverHttpError) as excinfo:
        await driver.http.request(
            "POST", "/api/session/create", body={"session_id": session_id}
        )
    assert excinfo.value.status == 404
    assert excinfo.value.call.response.get("error") == "session_archived"
    assert not probe.env.session_dir(session_id).exists()
    assert session_id in {e["id"] for e in await _archived_entries(probe)}

    # memory 后端没有归档区：409 拒绝，且会话保持原样。
    payload = await driver.http.request(
        "POST",
        "/api/session/create",
        body={"backend": "memory", "agent": {"model_id": CONFLICT_MODEL}},
    )
    memory_id = payload["session_id"]
    with pytest.raises(DriverHttpError) as excinfo:
        await _archive(probe, memory_id, archived=True)
    assert excinfo.value.status == 409
    assert "does not support archiving" in excinfo.value.call.response.get("detail", "")
    # 拒绝后会话原样：没有被移走（memory 会话无名无标，本来也不进列表）。
    assert memory_id not in {e["id"] for e in await _archived_entries(probe)}


@pytest.mark.probe_env(models=[SWEEP_MODEL], sessions=SWEEP_CONFIG)
@pytest.mark.timeout(180)
@pytest.mark.asyncio
async def test_sweep_archives_expired_sessions_with_pin_exemption(probe: Probe) -> None:
    """自动 sweep：过期归档、pin 豁免、内存中的过期会话先逐出再移动。"""
    stale = "20200101-000000-aaaaaaa1"
    pinned = "20200101-000000-aaaaaaa2"
    _seed_stale_session(probe, stale)
    _seed_stale_session(probe, pinned, tags=["pin"])

    probe.register(SWEEP_MODEL, Turn.of(text="reply"))
    fresh = await probe.session(model=SWEEP_MODEL)
    await fresh.chat("hi")
    driver = _driver(probe)
    # 新建的会话不在豁免保护之下（只有年龄保护它）——断言才有区分度。
    await driver.http.unsubscribe(fresh.session_id, driver.client_id)

    # 过期种子被收走；pin 豁免（等它先被看到一次再断言"没被收"+ 再等几个周期）。
    await _wait_archived(probe, stale, expected=True)

    # 内存中的过期会话：拨旧时间戳 → sweep 逐出 + 移动。
    _rewrite_last_interaction(probe, fresh.session_id)
    await _wait_archived(probe, fresh.session_id, expected=True)
    assert not probe.env.session_dir(fresh.session_id).exists(), (
        "被归档会话的 live 目录复活了（数据分裂红线）"
    )

    # 再等两个扫描周期：pin 的、新的都不动。
    await asyncio.sleep(1.2)
    ids = {entry["id"] for entry in await _archived_entries(probe)}
    assert ids == {stale, fresh.session_id}, ids
    live = await _live_ids(probe)
    assert pinned in live
    assert ".archive" not in live


@pytest.mark.probe_env(models=[DISABLED_MODEL], sessions=DISABLED_CONFIG)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_sweep_disabled_by_non_positive_threshold(probe: Probe) -> None:
    """``idle_days <= 0`` = 关闭：扫多个周期也不动任何会话。"""
    stale = "20200101-000000-bbbbbbb1"
    _seed_stale_session(probe, stale)

    await asyncio.sleep(2.5)  # 数个扫描周期

    assert [e["id"] for e in await _archived_entries(probe)] == []
    assert not (probe.env.sessions_path / ".archive").exists()
    assert stale in await _live_ids(probe)
