"""会话列表排序场景：活跃（已在内存）优先 + 组内最后交互时间降序。

``GET /api/session/list`` 的顺序是**唯一事实来源**——前端（TUI `/session` 面板、
``wing ps``）按原序渲染，不再用 workspace 匹配 / 状态优先级重排。本场景把该契约
钉在公开 HTTP 协议上：TUI 的 workspace 重排正是从这里看不见的地方长出来的。

覆盖的断言点：

- ``test_active_sessions_rank_before_disk_only_ones``：唯一能区分「活跃优先」与
  「纯时间降序」的形状——**active 的时间更旧、inactive 的时间更新**，active 仍
  在前。两组的 workspace 互不相同，顺序不受影响；
- 同一场景的后半段（同一个环境、同一批会话）：``resume`` 把磁盘上的会话拉回
  内存 → 它立刻升进 active 组；再取列表，顺序变成「最新的 active 在前、inactive
  垫底」——证明每次请求都按当前状态重算，而不是缓存的旧顺序；
- ``test_list_order_follows_last_interaction_when_nothing_is_loaded``：全部逐出后
  退化成纯时间降序（active/inactive 的分组边界不引入任何额外优先级）。

时间口径：``metadata.last_interaction`` 由每次 ``post()``（chat）推进，两次之间
``asyncio.sleep`` 保证严格可分辨；场景先对账四个时间戳本身严格递增（前置条件
失败也给得出原因），再对账列表顺序。

环境：标准 probe 配置（不压 TTL）——「active」由订阅维持在场，会话之间的时间差
不会被逐出扫描抢跑。
"""

from __future__ import annotations

import asyncio
from datetime import datetime

import pytest

from wing_probe import Driver, Probe, Session, Turn

#: 唯一剧本（4 轮 = 4 个会话各一次 chat，按序消费）。
ORDER_MODEL = "probe/session-list-order"

#: 状态翻转的轮询预算（本地 HTTP，实测亚秒级；放宽只为 CI 抖动）。
POLL_DEADLINE = 20.0
POLL_INTERVAL = 0.05

#: 两次 chat 之间的最小间隔：让 ``last_interaction`` 严格可分辨（µs 分辨率 +
#: 顺序 HTTP 往返，50ms 有数量级余量）。
SETTLE = 0.05


def _driver(probe: Probe) -> Driver:
    return probe.driver_required


def _rows(payload: dict) -> list[dict]:
    rows = payload.get("sessions")
    assert isinstance(rows, list), payload
    return rows


async def _list(probe: Probe) -> list[dict]:
    """``GET /api/session/list`` 的原始条目（跨 store 聚合的完整列表）。"""
    return _rows(await _driver(probe).http.request("GET", "/api/session/list"))


async def _status(probe: Probe, session_id: str) -> str | None:
    for entry in await _list(probe):
        if entry.get("id") == session_id:
            return entry.get("status")
    return None


async def _wait_status(probe: Probe, session_id: str, expected: str) -> None:
    """轮询到该会话的运行时状态变成 ``expected``（超时即失败，报告带最后观察值）。

    两个用途：① 等 ``turn_result`` 之后的 ``working → idle`` 落地（``release``
    要求 idle，不等就是撞 409 的竞态）；② 等逐出落地（``released: true`` 只是
    请求被接受，状态面以 list 为准）。
    """
    observed: str | None = None
    deadline = asyncio.get_running_loop().time() + POLL_DEADLINE
    while asyncio.get_running_loop().time() < deadline:
        observed = await _status(probe, session_id)
        if observed == expected:
            return
        await asyncio.sleep(POLL_INTERVAL)
    raise AssertionError(
        f"session {session_id} stayed in status {observed!r} for "
        f"{POLL_DEADLINE:.0f}s, expected {expected!r}"
    )


async def _open(probe: Probe, model: str, label: str, text: str) -> Session:
    """建一个会话、聊一轮、等它落回 idle（= active 组的成员）。

    每个会话给一个**自己的 workspace**：workspace 不再参与排序，这点由「四组
    workspace 全不同、顺序仍只由状态与时间决定」体现。
    """
    session = await probe.session(model=model, workspace=probe.env.root / "ws" / label)
    await session.chat(text)
    await _wait_status(probe, session.session_id, "idle")
    await asyncio.sleep(SETTLE)
    return session


async def _evict(probe: Probe, session: Session) -> None:
    """显式逐出（离开内存 = 转 ``inactive``）：断开订阅 → ``POST /api/session/release``。"""
    driver = _driver(probe)
    await driver.http.unsubscribe(session.session_id, driver.client_id)
    payload = await driver.http.request(
        "POST", "/api/session/release", body={"session_id": session.session_id}
    )
    assert payload == {"ok": True, "released": True, "detail": "released"}, payload
    await _wait_status(probe, session.session_id, "inactive")


def _timestamps(probe_rows: list[dict]) -> dict[str, datetime]:
    """会话 id → 解析后的 ``last_interaction``（缺字段直接失败）。"""
    stamps: dict[str, datetime] = {}
    for entry in probe_rows:
        raw = entry.get("last_interaction")
        assert isinstance(raw, str) and raw, entry
        stamps[str(entry["id"])] = datetime.fromisoformat(raw)
    return stamps


def _assert_groups(rows: list[dict], expected: list[str]) -> None:
    """顺序逐项对账，并在失败信息里带上 status / last_interaction（一次看清）。"""
    observed = [str(entry["id"]) for entry in rows]
    detail = [
        (entry["id"], entry.get("status"), entry.get("last_interaction"))
        for entry in rows
    ]
    assert observed == expected, f"expected {expected}, got {detail}"


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_active_sessions_rank_before_disk_only_ones(probe: Probe) -> None:
    """活跃优先压过时间；resume 把会话拉回内存后顺序按新状态重算。"""
    probe.register(
        ORDER_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text="reply two"),
        Turn.of(text="reply three"),
        Turn.of(text="reply four"),
    )

    # 时间轴：active_old(t0) < inactive_old(t1) < inactive_new(t2) < active_new(t3)
    active_old = await _open(probe, ORDER_MODEL, "active-old", "chat active old")
    inactive_old = await _open(probe, ORDER_MODEL, "inactive-old", "chat inactive old")
    inactive_new = await _open(probe, ORDER_MODEL, "inactive-new", "chat inactive new")
    active_new = await _open(probe, ORDER_MODEL, "active-new", "chat active new")
    await _evict(probe, inactive_old)
    await _evict(probe, inactive_new)

    rows = await _list(probe)
    stamps = _timestamps(rows)

    # 前置条件：四个会话的时间戳严格递增——「时间」这把尺子本身先立住。
    assert stamps[active_old.session_id] < stamps[inactive_old.session_id]
    assert stamps[inactive_old.session_id] < stamps[inactive_new.session_id]
    assert stamps[inactive_new.session_id] < stamps[active_new.session_id]

    # ① 活跃优先：inactive_new 是**全局最新**的会话，仍然排在时间更旧的
    #    active_old 之后（纯时间降序会给出 inactive_new 第一）。
    # ② 组内时间降序：active 组 [新建的, 旧的]、inactive 组 [新的, 旧的]。
    _assert_groups(
        rows,
        [
            active_new.session_id,
            active_old.session_id,
            inactive_new.session_id,
            inactive_old.session_id,
        ],
    )
    assert [entry["status"] for entry in rows] == [
        "idle",
        "idle",
        "inactive",
        "inactive",
    ]

    # resume = 把磁盘上的会话拉回内存：它立刻升进 active 组（时间仍是 t1）。
    await probe.resume(inactive_old.session_id)
    rows_after = await _list(probe)
    _assert_groups(
        rows_after,
        [
            active_new.session_id,  # t3
            inactive_old.session_id,  # t1，刚水合
            active_old.session_id,  # t0
            inactive_new.session_id,  # t2，唯一还在磁盘上的
        ],
    )
    assert [entry["status"] for entry in rows_after] == [
        "idle",
        "idle",
        "idle",
        "inactive",
    ]


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_list_order_follows_last_interaction_when_nothing_is_loaded(
    probe: Probe,
) -> None:
    """空内存（全部逐出）时退化成纯时间降序：分组边界不引入额外优先级。"""
    probe.register(
        ORDER_MODEL,
        Turn.of(text="reply one"),
        Turn.of(text="reply two"),
    )

    older = await _open(probe, ORDER_MODEL, "older", "chat older")
    newer = await _open(probe, ORDER_MODEL, "newer", "chat newer")
    await _evict(probe, older)
    await _evict(probe, newer)

    rows = await _list(probe)
    _assert_groups(rows, [newer.session_id, older.session_id])
    assert [entry["status"] for entry in rows] == ["inactive", "inactive"]
