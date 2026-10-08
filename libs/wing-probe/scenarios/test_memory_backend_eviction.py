"""非持久后端（``backend: memory``）的逐出语义：钉住 + 不落盘 + 重启即消失。

`MemorySessionStore.durable = False` 同时决定了三条对外行为，今天只有 loopback
单测，整机口径没有：

1. **显式 release 被拒**（409，原因 ``non-durable store 'memory'``）——逐出等于
   销毁数据，不能仅凭"用户说 release"就执行；
2. **自动 sweep 也跳过它**（`_blocked_reason` 的 non-durable 分支先于 subscribed
   判定）——TTL 到期也不会被逐出；
3. **不落盘**：`<sessions>/<id>` 目录不存在；重启网关后会话消失（列表缺席 +
   resume 404），而同期创建的 file 会话必须仍在（对照组）。

覆盖的断言点：

- ``test_memory_session_is_pinned_against_release_and_sweep``（``sessions=
  FAST_EVICTION``：TTL 1s / sweep 0.5s）：建 memory 会话 → 目录不存在；**先断开
  订阅**（去掉"被订阅"这个钉住理由，只剩 non-durable）→ release 409 + 原因；
  等过 TTL + 扫描周期仍 ``idle``（= 仍在内存）；重新订阅后照常对话（没被半拆解）；
- ``test_memory_session_vanishes_on_restart_while_file_session_survives``（标准
  TTL）：同一 env 里建 memory + file 两个会话各聊一轮 → ``restart_gateway()`` →
  列表只有 file、memory 的 resume/release 都是 404、file 的 resume 成功且历史
  完整。

> 为什么不要"秒级 TTL 下 release 被拒"的反面（release 成功）：non-durable 是
> **无条件**钉住（不看 TTL），用例 1 的"等过 TTL 仍 idle"就是它的确定性断言。
"""

from __future__ import annotations

import asyncio

import pytest

from wing_probe import Driver, DriverHttpError, Probe, Turn

#: 逐出观察场景的环境旋钮（秒级 TTL；与 test_session_eviction 同款）。
FAST_EVICTION: dict = {
    "eviction": {"idle_ttl_seconds": 1.0, "sweep_interval_seconds": 0.5}
}

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
PINNED_MODEL = "probe/memory-pinned"
RESTART_MODEL = "probe/memory-restart"

#: 等过 TTL + 扫描周期的预算（1s TTL + 0.5s sweep，留足抖动余量）。
EVICTION_WINDOW = 2.5


def _driver(probe: Probe) -> Driver:
    return probe.driver_required


async def _status(probe: Probe, session_id: str) -> str | None:
    """``/api/session/list`` 里该会话的运行时状态（None = 不在列表里）。

    ``inactive`` = 仅存在于磁盘（未加载）；``idle`` = 在内存里且空闲。
    """
    payload = await _driver(probe).http.request("GET", "/api/session/list")
    for entry in payload.get("sessions", []):
        if entry.get("id") == session_id:
            return entry.get("status")
    return None


def _messages(probe: Probe, session_id: str) -> list[str]:
    """落盘链上的消息内容（file 后端的 history.jsonl）。"""
    return [message["content"] for message in probe.history(session_id).messages()]


@pytest.mark.probe_env(models=[PINNED_MODEL], sessions=FAST_EVICTION)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_memory_session_is_pinned_against_release_and_sweep(probe: Probe) -> None:
    """memory 会话：release 409（non-durable）、sweep 不逐出、照常可用。"""
    probe.register(PINNED_MODEL, Turn.of(text="reply one"), Turn.of(text="reply two"))
    session = await probe.session(model=PINNED_MODEL, backend="memory")
    assert session.response.get("backend") == "memory", session.response
    await session.chat("hello")

    # 不落盘：memory 后端连会话目录都不建（"重启即消失"的根因，直接可证）。
    assert not probe.env.session_dir(session.session_id).exists()
    assert await _status(probe, session.session_id) == "idle"

    # 先断开订阅：去掉"被订阅"这个钉住理由，剩下的唯一理由必须是 non-durable。
    driver = _driver(probe)
    await driver.http.unsubscribe(session.session_id, driver.client_id)
    with pytest.raises(DriverHttpError) as failure:
        await session.release()
    assert failure.value.status == 409, failure.value.call.render()
    detail = str(failure.value.call.response.get("detail", ""))
    assert "non-durable" in detail and "memory" in detail, failure.value.call.response

    # 自动 sweep 同样跳过它：等过 TTL + 至少一个扫描周期，仍在内存（idle）。
    await asyncio.sleep(EVICTION_WINDOW)
    assert await _status(probe, session.session_id) == "idle", (
        "non-durable 会话不得被空闲逐出（逐出等于销毁数据）"
    )

    # 没被半拆解：重新订阅后照常对话。
    await driver.http.subscribe(session.session_id, driver.client_id)
    result = await session.chat("again")
    assert result.data["subtype"] == "success", result.data

    # memory 会话**没有磁盘视图**（history.jsonl 不存在）：事实看内存态
    # （`/api/session/get` 的 messages）与下一轮请求体（上下文真的带上了前一轮）。
    state = await session.get()
    assert [message["content"] for message in state["messages"]] == [
        "hello",
        "reply one",
        "again",
        "reply two",
    ], state
    follow_up = probe.request(PINNED_MODEL, 1)
    assert [(m.role, m.content) for m in follow_up.context().messages] == [
        ("user", "hello"),
        ("assistant", "reply one"),
        ("user", "again"),
    ], follow_up.describe()


@pytest.mark.probe_env(models=[RESTART_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_memory_session_vanishes_on_restart_while_file_session_survives(
    probe: Probe,
) -> None:
    """重启：memory 会话消失（列表缺席 + resume 404），file 会话仍在且历史完整。"""
    probe.register(RESTART_MODEL, Turn.of(text="memory reply"), Turn.of(text="file r"))
    memory_session = await probe.session(model=RESTART_MODEL, backend="memory")
    file_session = await probe.session(model=RESTART_MODEL, backend="file")
    await memory_session.chat("memory hello")
    await file_session.chat("file hello")

    # 前置条件：同 env、同模型的两个会话，落盘形态截然不同。
    assert not probe.env.session_dir(memory_session.session_id).exists()
    assert probe.env.session_dir(file_session.session_id).is_dir()

    await probe.restart_gateway()

    listed = await _driver(probe).http.list_sessions()
    ids = {entry["id"] for entry in listed.get("sessions", [])}
    assert memory_session.session_id not in ids, listed
    assert file_session.session_id in ids, listed

    # memory：跨进程不存在（内存 store 随进程消失）——resume 404。
    with pytest.raises(DriverHttpError) as resume_failure:
        await probe.resume(memory_session.session_id)
    assert resume_failure.value.status == 404, resume_failure.value.call.render()
    # 也不在任何 store 里：release 404（不是"幂等的 not loaded"）。
    with pytest.raises(DriverHttpError) as release_failure:
        await _driver(probe).http.release_session(memory_session.session_id)
    assert release_failure.value.status == 404, release_failure.value.call.render()

    # 对照：file 会话从磁盘水合，历史逐字完整。
    resumed = await probe.resume(file_session.session_id)
    assert _messages(probe, resumed.session_id) == ["file hello", "file r"]
    assert await _status(probe, file_session.session_id) == "idle"
