"""会话实时状态端点（``GET /api/session/live``）场景。

被守的契约（`docs/dev/http-api.md` 的 `GET /api/session/live` 行）：

- **行集 = 此刻在内存（已水合）的会话**，行形状只有 ``{id, status}``；
  未加载 / 已逐出的会话**不出现在响应里**（不是以 ``inactive`` 出现）；
- ``status ∈ {idle, working, waiting}``，是即时读取的运行状态——**不需要订阅**；
- 行集是 ``/api/session/list`` 活跃组的**超集**（list 隐藏无名且无标的条目，
  如刚创建还没发言的空会话），交集的相对顺序一致（组内时间降序 + id 定序）；
- **只读**：不水合、不触盘、不改变任何会话状态（对已逐出会话反复读也不会把它
  换入内存）。

存在理由（为什么不是「全量 list 就够了」）：会话列表 / rail 的状态及时性靠
"只补状态"的轮询，而 list 是逐会话读盘的全量枚举（跨 store 聚合 + 模型绑定
解析）；状态变更**不进**全局 WS 流（会话事件只发给订阅者），因此"没订阅的
观察者"要看到状态只能走 HTTP。本场景的观测端刻意是一个**只走 HTTP、从不订阅
任何会话**的客户端——它就是 rail 的替身。
"""

from __future__ import annotations

import asyncio
import contextlib
import time
from collections.abc import AsyncIterator

import pytest

from wing_probe import Probe, ToolCall, Turn
from wing_probe.driver import DriverHttp, DriverHttpError

TRANSITIONS_MODEL = "probe/live-transitions"
WAITING_MODEL = "probe/live-waiting"
EVICTION_MODEL = "probe/live-eviction"
ALIGNMENT_MODEL = "probe/live-alignment"

#: 逐出观察用秒级 TTL（与 test_session_eviction.py 同款旋钮）。
FAST_EVICTION: dict = {
    "eviction": {"idle_ttl_seconds": 1.0, "sweep_interval_seconds": 0.5}
}

#: 状态翻转的轮询预算（轮次窗口 + 逐出 TTL 都留足余量）。
#: 断言靠"轮询到翻转"，不靠 sleep 运气。
POLL_DEADLINE = 20.0
POLL_INTERVAL = 0.2


@contextlib.asynccontextmanager
async def _observer(probe: Probe) -> AsyncIterator[DriverHttp]:
    """只走 HTTP、从不订阅任何会话的观测客户端（会话列表 / rail 一侧的替身）。"""
    http = DriverHttp(probe.env.gateway_url, started_at=probe.env.started_at)
    try:
        yield http
    finally:
        await http.close()


async def _live_rows(http: DriverHttp) -> list[dict]:
    """``GET /api/session/live`` 的原始行（不做归一化：形状也是契约）。"""
    payload = await http.request("GET", "/api/session/live")
    return payload["sessions"]


async def _live_status(http: DriverHttp, session_id: str) -> str | None:
    """该会话在 live 投影里的状态（None = 行不在投影里）。"""
    for row in await _live_rows(http):
        if row["id"] == session_id:
            return row["status"]
    return None


async def _wait_live_status(http: DriverHttp, session_id: str, expected: str) -> str:
    """轮询到 live 状态等于 ``expected``，返回**实测值**（超时即失败）。"""
    deadline = time.monotonic() + POLL_DEADLINE
    observed: str | None = None
    while time.monotonic() < deadline:
        observed = await _live_status(http, session_id)
        if observed is not None and observed == expected:
            return observed
        await asyncio.sleep(POLL_INTERVAL)
    raise AssertionError(
        f"live status of {session_id} stayed {observed!r} for {POLL_DEADLINE:.0f}s, "
        f"expected {expected!r}"
    )


async def _list_active(http: DriverHttp) -> list[dict]:
    """``GET /api/session/list`` 的活跃组（``{id, status}``，保持响应序）。"""
    payload = await http.request("GET", "/api/session/list")
    return [
        {"id": entry["id"], "status": entry["status"]}
        for entry in payload["sessions"]
        if entry["status"] != "inactive"
    ]


@pytest.mark.probe_env(models=[TRANSITIONS_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_live_reports_status_transitions_to_a_non_subscriber(
    probe: Probe,
) -> None:
    """未订阅的客户端也读得到即时状态：idle → working → idle。

    WHEN 另一端（本会话的订阅者）驱动一轮对话
    AND 观测端只走 HTTP、从未订阅任何会话
    THEN ``/api/session/live`` 在该轮里报 ``working``、收口后报 ``idle``；
    行键集精确等于 ``{id, status}``；同一刻 live 与 ``list`` 活跃组一致
    （本场景只有这一条会话且列表可见；超集关系见 alignment 场景）。

    这是本端点存在的理由：状态变更不进全局 WS 流，rail 只能靠 HTTP 轮询补状态。
    """
    probe.register(TRANSITIONS_MODEL, Turn.of(text="hello there", chunk=2, delay=1.2))
    session = await probe.session(model=TRANSITIONS_MODEL)

    async with _observer(probe) as observer:
        rows = await _live_rows(observer)
        assert rows == [{"id": session.session_id, "status": "idle"}], rows
        # 行形状是契约：加字段会改变所有消费方的解码面。
        assert set(rows[0]) == {"id", "status"}, rows[0]

        await session.send("hi")
        assert (
            await _wait_live_status(observer, session.session_id, "working")
            == "working"
        )

        # 与 list 的活跃组逐行一致（本场景只有一条会话且已发言 → 列表里可见、
        # 无"隐藏行"；超集关系与隐藏行由 alignment 场景钉住）。
        assert await _live_rows(observer) == await _list_active(observer)

        result = await session.watch.expect("turn_result", within=20.0)
        assert result.data["subtype"] == "success", result.data

        assert await _wait_live_status(observer, session.session_id, "idle") == "idle"
        assert await _live_rows(observer) == await _list_active(observer)


@pytest.mark.probe_env(models=[WAITING_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_live_reports_waiting_while_an_ask_is_pending(probe: Probe) -> None:
    """挂起 Ask 期间报 ``waiting``（优先级 waiting > working），答完回 ``idle``。

    状态不是二元的"跑不跑"：阻塞在用户输入上是独立状态——rail 靠它把
    「在等你回答」渲染成可区分的形态。观测端同样不订阅。
    """
    probe.register(
        WAITING_MODEL,
        Turn.of(
            tool_calls=[
                ToolCall(
                    "AskUserQuestion",
                    {
                        "questions": [
                            {
                                "id": "flavor",
                                "header": "Flavor",
                                "question": "Which flavor?",
                                "options": [{"label": "vanilla"}],
                            }
                        ]
                    },
                )
            ]
        ),
        Turn.of(text="thanks"),
    )
    session = await probe.session(model=WAITING_MODEL)

    async with _observer(probe) as observer:
        await session.send("pick something")
        ask = await session.watch.expect("ask", within=15)
        assert (
            await _wait_live_status(observer, session.session_id, "waiting")
            == "waiting"
        )

        await session.answer(ask, "flavor: vanilla")
        result = await session.watch.expect("turn_result", within=20.0)
        assert result.data["subtype"] == "success", result.data
        assert await _wait_live_status(observer, session.session_id, "idle") == "idle"


@pytest.mark.probe_env(models=[EVICTION_MODEL], sessions=FAST_EVICTION)
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_live_drops_evicted_session_and_never_hydrates(probe: Probe) -> None:
    """逐出后行从投影里消失（不是转 ``inactive``），且反复读 live 不会水合。

    WHEN 空闲会话被逐出（TTL 到期）
    THEN live 里没有它的行、list 仍报 ``inactive``；连续多次读 live 之后会话
    依旧不在内存（``session/get`` 404 是"没被水合"的独立观测面）；resume 后
    行以 ``idle`` 回来。
    """
    probe.register(EVICTION_MODEL, Turn.of(text="reply"))
    session = await probe.session(model=EVICTION_MODEL)
    await session.chat("hello")

    async with _observer(probe) as observer:
        assert await _wait_live_status(observer, session.session_id, "idle") == "idle"

        # 断开订阅 → TTL 到期逐出（live 行随之消失）。
        await probe.driver_required.http.unsubscribe(
            session.session_id, probe.driver_required.client_id
        )
        deadline = time.monotonic() + POLL_DEADLINE
        while time.monotonic() < deadline:
            if await _live_status(observer, session.session_id) is None:
                break
            await asyncio.sleep(POLL_INTERVAL)
        else:
            raise AssertionError("evicted session still has a live row")

        # 逐出只丢内存态：list 仍列出它（inactive），live 不报 inactive 行。
        assert await _list_active(observer) == []
        assert await _live_rows(observer) == []

        # 只读：反复读 live 不会把它水合回内存（session/get 仍 404）。
        for _ in range(3):
            assert await _live_rows(observer) == []
        with pytest.raises(DriverHttpError) as excinfo:
            await observer.request(
                "GET", "/api/session/get", params={"session_id": session.session_id}
            )
        assert excinfo.value.status == 404, excinfo.value.call.response

        # 显式水合（resume）→ 行以 idle 回来。
        await probe.resume(session.session_id)
        assert await _wait_live_status(observer, session.session_id, "idle") == "idle"


@pytest.mark.probe_env(models=[ALIGNMENT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_live_order_aligns_with_the_list_active_group(probe: Probe) -> None:
    """live 是 list 活跃组的超集：交集逐行一致，多出的是"还没落盘"的新会话。

    WHEN 两条会话各自跑完一轮（B 更晚交互）
    AND 再建一条还没发言的空会话（无名无标、磁盘无痕迹）
    THEN live 行集 ⊇ list 活跃组（空会话只有 live 有——list 隐藏无名无标条目），
    且交集在 live 里的相对顺序与 list 的活跃组一致（按 id 合并安全，按下标
    zip 会错位）。空会话自身的排位由「缺失 last_interaction → id 前缀」回退
    决定，不在此断言（见 `TestListLiveSessions` 的排序用例）。
    """
    probe.register(ALIGNMENT_MODEL, Turn.of(text="reply a"), Turn.of(text="reply b"))
    first = await probe.session(model=ALIGNMENT_MODEL)
    await first.chat("a")
    second = await probe.session(model=ALIGNMENT_MODEL)
    await second.chat("b")
    empty = await probe.session(model=ALIGNMENT_MODEL)

    async with _observer(probe) as observer:
        active = await _list_active(observer)
        assert [row["id"] for row in active] == [second.session_id, first.session_id]

        rows = await _live_rows(observer)
        by_id = {row["id"]: row for row in rows}
        assert set(by_id) == {empty.session_id, second.session_id, first.session_id}, (
            rows
        )
        assert set(by_id) > {row["id"] for row in active}, (
            rows
        )  # 空会话是"多出来"的那行

        # 交集在 live 里的**相对顺序** == list 活跃组的顺序（行序也是契约，不只是集合）。
        active_ids = {row["id"] for row in active}
        assert [row["id"] for row in rows if row["id"] in active_ids] == [
            row["id"] for row in active
        ]
        assert [by_id[row["id"]] for row in active] == active
