"""用量聚合端点（``GET /api/usage``）的确定性场景：真网关 + 假 Provider。

被守的契约（``docs/dev/usage.md`` 的端点一节，实现落点
``libs/core/wing/{metrics,gateway/routes/usage.py}``）：

- **数字是相对关系**：剧本给了多少次调用 / 多少 token，端点就报多少（不硬编码
  绝对量，剧本改了断言不用改）；可加度量的行和 == ``totals``（TOTAL 行可对账）；
- **空数据永远 200**（不是 404、不是 500）：全零 totals + 稠密 rows；
- **会话隔离**：``session_id`` 只含该会话的数，别处的一分不少也不多；
- **库降级仍 200**：``wing.db`` 打不开时统计全零，网关照常跑（统计是旁路，
  不能把端点变成 500）；
- **不水合**：调端点不会把已逐出的会话读回内存（``release`` 之后仍 ``inactive``）；
- **血缘**：带父创建的会话落在 ``origin=agents``，根会话落在 ``me``。

禁止 import ``wing``（probe 门禁）：一切经公开 HTTP 观测，落库事实用**只读**
sqlite3 连接交叉对账。
"""

from __future__ import annotations

import sqlite3
from datetime import date
from pathlib import Path

import pytest

from wing_probe import Probe, ToolCall, Turn, Usage

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
DATA_MODEL = "probe/usage-data"
DEGRADED_MODEL = "probe/usage-degraded"
LINEAGE_MODEL = "probe/usage-lineage"
#: 只读端点不经过模型，但空库场景仍需要一个可启动的模型配置。
IDLE_MODEL = "probe/usage-idle"

#: 可加度量（``sessions`` 是去重计数，单日窗口下才与行相等）。
ADDITIVE = (
    "sessions_created",
    "user_messages",
    "llm_calls",
    "tool_calls",
    "tool_errors",
    "prompt_tokens",
    "completion_tokens",
    "cached_tokens",
    "llm_time_ms",
    "tool_time_ms",
)


def _read_only_connect(path: Path) -> sqlite3.Connection:
    """只读连接（``mode=ro``：库里没有这张表时明确报错，而不是悄悄建个空库）。"""
    conn = sqlite3.connect(f"{path.as_uri()}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    return conn


async def _usage(probe: Probe, **params: object) -> dict:
    """``GET /api/usage``（缺省 since=all、until=今天：窗口永远覆盖本次场景）。"""
    query: dict[str, object] = {"since": "all", "until": date.today().isoformat()}
    query.update(params)
    return await probe.driver_required.http.request("GET", "/api/usage", params=query)


def _row(body: dict, key: str) -> dict:
    for row in body["rows"]:
        if row["key"] == key:
            return row
    raise AssertionError(f"no row {key!r} in {[row['key'] for row in body['rows']]}")


@pytest.mark.probe_env(models=[DATA_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_usage_reports_the_turn_it_just_ran(probe: Probe) -> None:
    """一轮带工具调用的对话 → 端点报出这轮的调用数 / token / 工具，且与库一致。

    WHEN 两帧 usage（tool 轮 + 收尾轮）+ 一次 Bash 调用
    THEN ``group_by=day`` 的今日行：llm_calls=2 / tool_calls=1 / 三桶 token 累加 /
    user_messages=1 / sessions=1 / sessions_created=1；totals 与行逐字段相等；
    只读 sqlite3 交叉对账：audit_llm_call 两行、audit_tool_call 一行、
    audit_user_message 一行、session_catalog 一行（根会话）。
    """
    probe.register(
        DATA_MODEL,
        Turn.of(
            tool_calls=[ToolCall("Bash", {"command": "echo probe-usage"})],
            usage=Usage(prompt_tokens=100, completion_tokens=20, cached_tokens=40),
            delay=0.02,
        ),
        Turn.of(
            text="done",
            usage=Usage(prompt_tokens=150, completion_tokens=5),
            delay=0.02,
        ),
    )
    session = await probe.session(model=DATA_MODEL, yolo=True)

    result = await session.chat("run it")

    assert result.data["subtype"] == "success", result.data
    session.watch.assert_never("error")

    body = await _usage(probe, group_by="day")
    today = _row(body, date.today().isoformat())
    assert today["llm_calls"] == 2
    assert today["tool_calls"] == 1
    assert today["tool_errors"] == 0
    assert today["prompt_tokens"] == 250
    assert today["completion_tokens"] == 25
    assert today["cached_tokens"] == 40
    assert today["user_messages"] == 1
    assert today["sessions"] == 1
    assert today["sessions_created"] == 1
    assert today["llm_time_ms"] > 0
    assert today["tool_time_ms"] >= 0

    # totals 是独立的全量查询：单日窗口下与行逐字段相等（可加度量）
    for name in ADDITIVE:
        assert body["totals"][name] == today[name], name
    assert body["totals"]["sessions"] == 1

    # 交叉对账：端点报的数就是库里那几行（不经过端点读原始事实）
    conn = _read_only_connect(probe.env.wing_home / "core" / "wing.db")
    try:
        llm = conn.execute("SELECT * FROM audit_llm_call").fetchall()
        tools = conn.execute("SELECT * FROM audit_tool_call").fetchall()
        user_messages = conn.execute("SELECT * FROM audit_user_message").fetchall()
        catalog = conn.execute("SELECT * FROM session_catalog").fetchall()
    finally:
        conn.close()
    assert len(llm) == 2
    assert len(tools) == 1
    assert len(user_messages) == 1
    assert {row["session_id"] for row in llm} == {session.session_id}
    assert [row["session_id"] for row in user_messages] == [session.session_id]
    assert len(catalog) == 1
    assert catalog[0]["session_id"] == session.session_id
    assert catalog[0]["parent_session_id"] is None
    assert catalog[0]["created_source"] == "create"


@pytest.mark.probe_env(models=[IDLE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_empty_database_is_200_with_zeros(probe: Probe) -> None:
    """全新 ``WING_HOME``（没有任何会话）→ 200 + 全零 totals + 稠密 rows。

    空数据不是错误：消费方（CLI / TUI）按同一条路径渲染「还没有数据」。
    """
    body = await _usage(probe)

    assert body["totals"]["sessions"] == 0
    assert body["totals"]["llm_calls"] == 0
    assert body["totals"]["prompt_tokens"] == 0
    assert body["totals"]["sessions_created"] == 0
    # 库完全为空时 `since=all` 收敛到 until 当天：仍然是稠密的一行
    assert [row["key"] for row in body["rows"]] == [date.today().isoformat()]
    assert body["truncated"] is False


@pytest.mark.probe_env(models=[DATA_MODEL, LINEAGE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_session_scope_is_isolated(probe: Probe) -> None:
    """两个会话各跑一轮 → ``session_id`` 只含自己的数，全局是两者之和。"""
    probe.register(
        DATA_MODEL,
        Turn.of(text="a", usage=Usage(prompt_tokens=100, completion_tokens=10)),
        Turn.of(text="b", usage=Usage(prompt_tokens=300, completion_tokens=30)),
    )
    first = await probe.session(model=DATA_MODEL, yolo=True)
    second = await probe.session(model=DATA_MODEL, yolo=True)
    assert (await first.chat("one")).data["subtype"] == "success"
    assert (await second.chat("two")).data["subtype"] == "success"

    alone = await _usage(probe, group_by="session", session_id=first.session_id)
    whole = await _usage(probe, group_by="session")

    assert [row["key"] for row in alone["rows"]] == [first.session_id]
    assert alone["totals"]["llm_calls"] == 1
    assert alone["totals"]["prompt_tokens"] == 100
    assert alone["totals"]["sessions"] == 1
    assert whole["totals"]["llm_calls"] == 2
    assert whole["totals"]["prompt_tokens"] == 400
    assert whole["totals"]["sessions"] == 2
    assert {row["key"] for row in whole["rows"]} == {
        first.session_id,
        second.session_id,
    }


@pytest.mark.probe_env(models=[LINEAGE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_unknown_session_is_404(probe: Probe) -> None:
    """未知 session id → 404（0 是一个读数，「打错了」不是）。"""
    with pytest.raises(Exception) as failure:
        await _usage(probe, group_by="session", session_id="no-such-session")

    status = getattr(getattr(failure.value, "call", None), "status", None)
    assert status == 404, f"expected 404, got {failure.value!r}"


@pytest.mark.probe_env(models=[DEGRADED_MODEL], pre_start_dirs=["core/wing.db"])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_unopenable_database_still_answers(probe: Probe) -> None:
    """``wing.db`` 打不开（库路径被占成目录）→ 端点仍 200 + 全零，网关照常跑。

    统计是旁路：库降级时既不能把端点变成 500，也不能影响对话本身。
    """
    assert (probe.env.wing_home / "core" / "wing.db").is_dir()

    probe.register(
        DEGRADED_MODEL,
        Turn.of(text="survived", usage=Usage(prompt_tokens=10, completion_tokens=1)),
    )
    session = await probe.session(model=DEGRADED_MODEL, yolo=True)
    assert (await session.chat("hi")).data["subtype"] == "success"

    body = await _usage(probe, group_by="day")

    assert body["totals"]["llm_calls"] == 0
    assert body["totals"]["prompt_tokens"] == 0
    assert all(row["llm_calls"] == 0 for row in body["rows"])


@pytest.mark.probe_env(models=[DATA_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_usage_does_not_hydrate_the_session(probe: Probe) -> None:
    """只读库：读统计不会把已逐出的会话读回内存（``inactive`` 保持不变）。"""
    probe.register(
        DATA_MODEL,
        Turn.of(text="done", usage=Usage(prompt_tokens=50, completion_tokens=5)),
    )
    session = await probe.session(model=DATA_MODEL, yolo=True)
    assert (await session.chat("hi")).data["subtype"] == "success"

    # 订阅会钉住会话（release 因此 409）：先退订再逐出。
    driver = probe.driver_required
    await driver.http.unsubscribe(session.session_id, driver.client_id)
    await session.release()
    body = await _usage(probe, group_by="session", session_id=session.session_id)
    assert body["totals"]["llm_calls"] == 1

    listed = await driver.http.request("GET", "/api/session/list")
    status = {row["id"]: row["status"] for row in listed["sessions"]}[
        session.session_id
    ]
    assert status == "inactive"


@pytest.mark.probe_env(models=[LINEAGE_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_lineage_splits_me_and_agents(probe: Probe) -> None:
    """带父创建的子会话落在 ``origin=agents``；根会话落在 ``me``。"""
    probe.register(
        LINEAGE_MODEL,
        Turn.of(text="root", usage=Usage(prompt_tokens=10, completion_tokens=1)),
        Turn.of(text="child", usage=Usage(prompt_tokens=90, completion_tokens=9)),
    )
    driver = probe.driver_required
    root = await probe.session(model=LINEAGE_MODEL, yolo=True)
    assert (await root.chat("root turn")).data["subtype"] == "success"

    created = await driver.http.request(
        "POST",
        "/api/session/create",
        body={
            "parent_session_id": root.session_id,
            "agent": {"model_id": LINEAGE_MODEL, "yolo": True},
        },
    )
    child_id = created["session_id"]
    child = await driver.attach(child_id)
    assert (await child.chat("child turn")).data["subtype"] == "success"

    body = await _usage(probe, group_by="origin")

    assert [row["key"] for row in body["rows"]] == ["me", "agents"]
    me, agents = body["rows"]
    assert me["llm_calls"] == 1
    assert me["prompt_tokens"] == 10
    assert agents["llm_calls"] == 1
    assert agents["prompt_tokens"] == 90
    assert agents["sessions"] == 1
    assert body["totals"]["prompt_tokens"] == 100
    assert body["totals"]["sessions_created"] == 2
