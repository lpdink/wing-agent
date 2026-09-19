"""session 逐出（eviction）红线场景。

事实口径：**session 的内存态是缓存**——逐出只回收运行期资源（worker +
provider client），磁盘（history.jsonl / metadata.json）是事实来源。逐出后
任何"要求会话在场"的入口按需水合（resume / subscribe / send）。

覆盖的断言点：

- ``test_idle_session_is_evicted_then_hydrates_on_demand``：空闲 + 无人订阅
  的会话超过 TTL 被逐出——``/api/session/list`` 的 status 由 ``idle`` 变
  ``inactive`` 是逐出对外**唯一可见痕迹**；逐出不动磁盘（链完整）；对已逐出
  会话的 release 幂等返回 ``not loaded``；重新订阅即水合，下一轮对话正常且
  上下文不变形（**红线**：逐出/水合不改变上下文链）；
- ``test_subscribed_session_is_never_evicted``：订阅中的会话是用户工作集——
  TTL 到期仍不逐出（status 保持 ``idle``），显式 release 被 409 拒绝并给出
  原因（钉住条件：被订阅）；
- ``test_release_evicts_idle_session``：显式 release 立即逐出（不等 TTL）——
  首次 ``released=true``、``list`` 转 ``inactive``；再次调用幂等。

环境：``probe_env`` 标记覆盖 ``sessions.eviction``：
- 逐出观察用 TTL 1s / 扫描 0.5s（默认 30min 在场景里不可等待）；
- release 场景用 TTL 30s / 扫描 0.2s（release 路径不依赖 TTL，长 TTL 让
  "扫描先抢跑"不可能发生，断言确定性来自配置而不是运气）。
"""

from __future__ import annotations

import asyncio
import time

import pytest

from wing_probe import Probe, Turn
from wing_probe.driver import DriverHttpError
from wing_probe.driver.session import Driver

#: 逐出观察场景的环境旋钮（秒级 TTL）。
FAST_EVICTION: dict = {
    "eviction": {"idle_ttl_seconds": 1.0, "sweep_interval_seconds": 0.5}
}

#: release 场景的环境旋钮（TTL 远大于场景时长）。
SLOW_EVICTION: dict = {
    "eviction": {"idle_ttl_seconds": 30.0, "sweep_interval_seconds": 0.2}
}

HYDRATE_MODEL = "probe/eviction-hydrate"
PINNED_MODEL = "probe/eviction-pinned"
RELEASE_MODEL = "probe/eviction-release"

#: 状态翻转的轮询预算（TTL 1s + 扫描 0.5s，留足日志/CJ 抖动余量）。
POLL_DEADLINE = 20.0
POLL_INTERVAL = 0.2


def _driver(probe: Probe) -> Driver:
    return probe.driver_required


async def _session_status(probe: Probe, session_id: str) -> str | None:
    """``/api/session/list`` 里该会话的运行时状态（None = 不在列表里）。"""
    payload = await _driver(probe).http.request("GET", "/api/session/list")
    for entry in payload.get("sessions", []):
        if entry.get("id") == session_id:
            return entry.get("status")
    return None


async def _wait_status(probe: Probe, session_id: str, expected: str) -> str:
    """轮询到状态等于 ``expected``（超时即失败，报告带最后观察值）。"""
    deadline = time.monotonic() + POLL_DEADLINE
    observed: str | None = None
    while time.monotonic() < deadline:
        observed = await _session_status(probe, session_id)
        if observed == expected:
            return expected
        await asyncio.sleep(POLL_INTERVAL)
    raise AssertionError(
        f"session {session_id} stayed in status {observed!r} for "
        f"{POLL_DEADLINE:.0f}s, expected {expected!r}"
    )


async def _release(probe: Probe, session_id: str) -> dict:
    """``POST /api/session/release``（错误由 DriverHttpError 带出）。"""
    return await _driver(probe).http.request(
        "POST", "/api/session/release", body={"session_id": session_id}
    )


@pytest.mark.probe_env(sessions=FAST_EVICTION)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_idle_session_is_evicted_then_hydrates_on_demand(probe: Probe) -> None:
    """逐出 = 只丢内存态；重新订阅即水合，上下文不变形（红线）。"""
    probe.register(HYDRATE_MODEL, Turn.of(text="reply one"), Turn.of(text="reply two"))
    session = await probe.session(model=HYDRATE_MODEL)
    await session.chat("hello")

    # 在场（订阅中）时是 idle——逐出前的基线。
    assert await _session_status(probe, session.session_id) == "idle"
    before = probe.history(session)

    # 断开订阅：会话失去「在场理由」，TTL 到期被逐出。
    await _driver(probe).http.unsubscribe(session.session_id, _driver(probe).client_id)
    assert await _wait_status(probe, session.session_id, "inactive") == "inactive"

    # 逐出不动磁盘：链完整（红线的一半）。
    after_eviction = probe.history(session)
    assert [message["content"] for message in after_eviction.messages()] == [
        "hello",
        "reply one",
    ]
    assert [record["uuid"] for record in after_eviction.records] == [
        record["uuid"] for record in before.records
    ]

    # 幂等：release 一个已被逐出的会话 → not loaded（200，不是错误）。
    release_call = await _release(probe, session.session_id)
    assert release_call == {"ok": True, "released": False, "detail": "not loaded"}
    last_release = probe.last_http_call(path="/api/session/release")
    assert last_release is not None and last_release.status == 200

    # 重新订阅 → 按需水合（订阅即恢复直播）。
    await _driver(probe).http.subscribe(session.session_id, _driver(probe).client_id)
    assert await _session_status(probe, session.session_id) == "idle"

    # 水合后照常对话：新上下文 = 原上下文 + 新一轮（红线：链不变形）。
    await session.chat("again")
    final = probe.history(session)
    assert [message["content"] for message in final.messages()] == [
        "hello",
        "reply one",
        "again",
        "reply two",
    ]


@pytest.mark.probe_env(sessions=FAST_EVICTION)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_subscribed_session_is_never_evicted(probe: Probe) -> None:
    """订阅中的会话是用户工作集：TTL 到期不逐出，显式 release 也被拒绝。"""
    probe.register(PINNED_MODEL, Turn.of(text="reply"), Turn.of(text="still here"))
    session = await probe.session(model=PINNED_MODEL)
    await session.chat("hello")

    # 等过 TTL + 至少一个扫描周期：状态仍是 idle（= 仍在内存）。
    await asyncio.sleep(2.0)
    assert await _session_status(probe, session.session_id) == "idle"

    # 显式 release 同样被钉住条件拒绝：409 + 原因。
    with pytest.raises(DriverHttpError) as excinfo:
        await _release(probe, session.session_id)
    assert excinfo.value.status == 409
    detail = excinfo.value.call.response.get("detail", "")
    assert "subscribed" in detail, excinfo.value.call.response

    # 拒绝后的会话仍在内存，照常可用。
    await session.chat("still here")
    assert [message["content"] for message in probe.history(session).messages()] == [
        "hello",
        "reply",
        "still here",
        "still here",
    ]


@pytest.mark.probe_env(sessions=SLOW_EVICTION)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_release_evicts_idle_session(probe: Probe) -> None:
    """显式 release 立即逐出（不为 TTL 等待），幂等且磁盘不动。"""
    probe.register(RELEASE_MODEL, Turn.of(text="reply"))
    session = await probe.session(model=RELEASE_MODEL)
    await session.chat("hello")

    await _driver(probe).http.unsubscribe(session.session_id, _driver(probe).client_id)

    release_call = await _release(probe, session.session_id)
    assert release_call == {"ok": True, "released": True, "detail": "released"}
    assert await _session_status(probe, session.session_id) == "inactive"

    # 幂等：再 release 一次 → not loaded；磁盘状态不动。
    assert await _release(probe, session.session_id) == {
        "ok": True,
        "released": False,
        "detail": "not loaded",
    }
    assert [message["content"] for message in probe.history(session).messages()] == [
        "hello",
        "reply",
    ]
