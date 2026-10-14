"""会话列表的子集过滤（``GET /api/session/list`` 的 ``status`` / ``tag`` 参数）场景。

被守的契约（`docs/dev/http-api.md` 的 `/api/session/list` 行）：

- ``status``：逗号分隔的状态过滤（``inactive`` / ``idle`` / ``working`` /
  ``waiting``）；``tag``：逗号分隔的标签过滤（AND —— 必须全部携带）；两者同时
  给出即 AND；
- **过滤 = 子集**：结果是全量列表的**子序列**（同样的行、同样的相对顺序，只是
  少了几行）——按"全量 + 谓词"计算必须逐行相等；
- 非法取值（未知状态 / 空段）一律 400；
- 缺省不过滤（行为与加参数之前逐字一致）。

为什么要有它（动机）：会话列表 / rail 的状态及时性靠**只取所需子集**的轮询——
``?status=idle,working,waiting`` 就是"在内存的会话"（能在跑的那批）的完整行，
不必每次拉全量（逐会话读盘的全量枚举）。状态变更不进全局 WS 流（会话事件只发给
订阅者），所以"没订阅的观察者"只能走 HTTP——本场景的观测端刻意是只走 HTTP、
从不订阅任何会话的客户端。
"""

from __future__ import annotations

import asyncio
import contextlib
import time
from collections.abc import AsyncIterator, Callable, Mapping

import pytest

from wing_probe import Probe, ToolCall, Turn
from wing_probe.driver import DriverHttp, DriverHttpError

FILTER_MODEL = "probe/list-filters"
RAIL_MODEL = "probe/list-filters-rail"
WAITING_MODEL = "probe/list-filters-waiting"

#: 状态翻转的轮询预算（轮次窗口留足余量）。
POLL_DEADLINE = 20.0
POLL_INTERVAL = 0.2

#: "在内存的会话"——列表的 status 过滤视角（= 能在跑的那批）。
ACTIVE_STATUSES = "idle,working,waiting"


@contextlib.asynccontextmanager
async def _observer(probe: Probe) -> AsyncIterator[DriverHttp]:
    """只走 HTTP、从不订阅任何会话的观测客户端（会话列表 / rail 一侧的替身）。"""
    http = DriverHttp(probe.env.gateway_url, started_at=probe.env.started_at)
    try:
        yield http
    finally:
        await http.close()


async def _list(http: DriverHttp, **filters: str) -> list[dict]:
    """``GET /api/session/list``（可带过滤）；返回原始的 ``sessions`` 行列表。"""
    params: Mapping[str, str] = {k: v for k, v in filters.items() if v is not None}
    payload = await http.request("GET", "/api/session/list", params=params or None)
    return payload["sessions"]


def _ids(rows: list[dict]) -> list[str]:
    return [row["id"] for row in rows]


async def _wait_status(http: DriverHttp, session_id: str, expected: str) -> str:
    """在 ``?status=idle,working,waiting`` 的投影里轮询到该会话状态为 expected。"""
    deadline = time.monotonic() + POLL_DEADLINE
    observed: str | None = None
    while time.monotonic() < deadline:
        observed = next(
            (
                row["status"]
                for row in await _list(http, status=ACTIVE_STATUSES)
                if row["id"] == session_id
            ),
            None,
        )
        if observed is not None and observed == expected:
            return observed
        await asyncio.sleep(POLL_INTERVAL)
    raise AssertionError(
        f"session {session_id} stayed {observed!r} in the active projection for "
        f"{POLL_DEADLINE:.0f}s, expected {expected!r}"
    )


async def _tag(http: DriverHttp, session_id: str, tags: list[str]) -> None:
    await http.request(
        "POST", "/api/session/tag", body={"session_id": session_id, "add": tags}
    )


@pytest.mark.probe_env(models=[FILTER_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_filters_select_a_subsequence_of_the_full_list(probe: Probe) -> None:
    """各过滤组合 == 全量列表 + 谓词（逐行相等，含行序）。

    WHEN 两条带标签的会话跑完一轮、其中一条被逐出（inactive）
    THEN ``status`` / ``tag`` / 两者组合的每个查询都与"全量 + 对应谓词"逐行相等
    （命中行的行序一致）；未知标签是空结果（200），不是错误。
    """
    probe.register(FILTER_MODEL, Turn.of(text="reply a"), Turn.of(text="reply b"))
    http = probe.driver_required.http

    crew = await probe.session(model=FILTER_MODEL)
    await crew.chat("a")
    both = await probe.session(model=FILTER_MODEL)
    await both.chat("b")
    await _tag(http, crew.session_id, ["crew"])
    await _tag(http, both.session_id, ["crew", "pin"])

    # 一条退到盘上（inactive）：过滤要能穿过 active / inactive 分组。
    # 打标让它在列表里可见（无名且无标的会话被列表隐藏——过滤不会凭空造行）。
    released = await probe.session(model=FILTER_MODEL)
    await _tag(http, released.session_id, ["crew"])
    await http.unsubscribe(released.session_id, probe.driver_required.client_id)
    assert (await http.release_session(released.session_id))["released"] is True

    async with _observer(probe) as observer:
        full = await _list(observer)
        assert {crew.session_id, both.session_id, released.session_id} <= set(
            _ids(full)
        )

        cases: list[tuple[dict[str, str], Callable[[dict], bool]]] = [
            ({"status": ACTIVE_STATUSES}, lambda r: r["status"] != "inactive"),
            ({"status": "inactive"}, lambda r: r["status"] == "inactive"),
            ({"status": "working"}, lambda r: r["status"] == "working"),
            ({"tag": "crew"}, lambda r: "crew" in r["tags"]),
            ({"tag": "crew,pin"}, lambda r: {"crew", "pin"} <= set(r["tags"])),
            (
                {"status": "inactive", "tag": "crew"},
                lambda r: r["status"] == "inactive" and "crew" in r["tags"],
            ),
        ]
        for filters, predicate in cases:
            filtered = await _list(observer, **filters)
            assert filtered == [row for row in full if predicate(row)], filters

        # 只逐出、带 crew 的那条 = 子集里恰好一行（锚定谓词真的在筛东西）。
        assert _ids(await _list(observer, status="inactive", tag="crew")) == [
            released.session_id
        ]
        # 未知标签：按字面匹配 → 空结果（不是 400）。
        assert await _list(observer, tag="no-such-tag") == []


@pytest.mark.probe_env(models=[RAIL_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_status_filter_observes_transitions_without_subscription(
    probe: Probe,
) -> None:
    """未订阅的观察者用 ``?status=idle,working,waiting`` 看得到 working → idle。

    这是过滤参数的动机面：rail 只取"在内存的会话"那批完整行轮询，不拉全量；
    状态变更不进全局 WS 流，所以"没订阅"不等于"看不到"。（过滤本身的语义由
    `test_filters_select_a_subsequence_of_the_full_list` 钉——本场景即使过滤被
    整体忽略也照样通过，它钉的是"未订阅观察者读得到即时状态"。）
    """
    probe.register(RAIL_MODEL, Turn.of(text="hello there", chunk=2, delay=1.2))
    session = await probe.session(model=RAIL_MODEL)
    # 打标让它在列表里可见（无名且无标的会话被列表隐藏）。
    await _tag(probe.driver_required.http, session.session_id, ["crew"])

    async with _observer(probe) as observer:
        assert session.session_id in _ids(await _list(observer, status=ACTIVE_STATUSES))

        await session.send("hi")
        assert await _wait_status(observer, session.session_id, "working") == "working"

        result = await session.watch.expect("turn_result", within=20.0)
        assert result.data["subtype"] == "success", result.data
        assert await _wait_status(observer, session.session_id, "idle") == "idle"


@pytest.mark.probe_env(models=[WAITING_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_waiting_is_queryable_while_an_ask_is_pending(probe: Probe) -> None:
    """挂起 Ask 期间 `?status=waiting` 命中该会话，且在活跃投影里报 waiting。

    状态的第三个取值同样可过滤（waiting > working 的优先级与 live 状态同源）——
    rail 想单独展示「等你回答」的那批时用它，不必拉全量再自己筛。
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
    await session.send("pick something")
    ask = await session.watch.expect("ask", within=15)

    async with _observer(probe) as observer:
        assert _ids(await _list(observer, status="waiting")) == [session.session_id]
        active = await _list(observer, status=ACTIVE_STATUSES)
        row = next(r for r in active if r["id"] == session.session_id)
        assert row["status"] == "waiting", row

    await session.answer(ask, "flavor: vanilla")
    result = await session.watch.expect("turn_result", within=20.0)
    assert result.data["subtype"] == "success", result.data

    async with _observer(probe) as observer:
        assert await _list(observer, status="waiting") == []
        assert await _wait_status(observer, session.session_id, "idle") == "idle"


@pytest.mark.probe_env(models=[FILTER_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_invalid_filter_values_are_rejected(probe: Probe) -> None:
    """未知状态 / 空段 → 400（静默忽略会让调用方以为过滤生效了）。"""
    async with _observer(probe) as observer:
        for filters in (
            {"status": "idle,bogus"},
            {"status": "active"},
            {"status": "idle,"},
            {"tag": "pin,"},
            {"tag": ""},
            {"tag": "-pin"},
            {"tag": "pin x"},
        ):
            with pytest.raises(DriverHttpError) as excinfo:
                await _list(observer, **filters)
            assert excinfo.value.status == 400, (filters, excinfo.value.call.response)

        # 重复参数同样 400（FastAPI 默认 last-wins 会静默丢值）。
        for name in ("status", "tag"):
            with pytest.raises(DriverHttpError) as excinfo:
                await observer.request(
                    "GET", "/api/session/list", params={name: ["idle", "working"]}
                )
            assert excinfo.value.status == 400, (name, excinfo.value.call.response)

        # 合法取值照常 200（对照组：不是"带参数就 400"）。
        assert await _list(observer, status="idle", tag="crew") == []
