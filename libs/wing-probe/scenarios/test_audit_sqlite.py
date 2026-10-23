"""审计落库场景：一次「LLM → 工具调用 → 收尾 LLM」之后，wing.db 里有对应的行。

覆盖的断言点：

- ``test_audit_rows_land_in_wing_db``：真网关跑一轮带工具调用的对话后，用 sqlite3
  **只读**连接直接读探针 ``WING_HOME/core/wing.db``——``audit_llm_call`` 两次调用
  各一行（model / provider / session_id / token 逐列正确）、``audit_tool_call``
  恰一行（tool_name / tool_call_id / success / duration_ms）；旧实现写的
  ``metrics.json`` 不再出现（写放大三件套已整体废弃）。
- ``test_unopenable_database_does_not_break_the_gateway``：库路径先被占成目录
  （sqlite3 打不开）——审计降级、一轮工具对话照样成功收口，且后端日志留一行
  ``wing.db disabled``（降级只在打开时留痕）。

为什么不经 HTTP 读：本期审计**只写不读**（没有观测端点），表结构就是这个特性的
唯一对外形状——直接读库才是在断言真实落盘结果。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, ToolCall, Turn, Usage, read_only_connect

#: 场景私有 model 名（剧本按 model 名路由，场景之间不共享）。
AUDIT_MODEL = "probe/audit-sqlite"
DEGRADED_MODEL = "probe/audit-degraded"


@pytest.mark.probe_env(models=[AUDIT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_audit_rows_land_in_wing_db(probe: Probe) -> None:
    """一轮带工具调用的对话 → audit_llm_call 两行 + audit_tool_call 一行。

    剧本：第一轮回一个 Bash 调用（带 usage），工具执行后再回正文收尾（带 usage）。
    """
    probe.register(
        AUDIT_MODEL,
        Turn.of(
            tool_calls=[ToolCall("Bash", {"command": "echo probe-audit"})],
            usage=Usage(prompt_tokens=100, completion_tokens=20, cached_tokens=40),
            delay=0.02,
        ),
        Turn.of(
            text="done",
            usage=Usage(prompt_tokens=150, completion_tokens=5),
            delay=0.02,
        ),
    )
    session = await probe.session(model=AUDIT_MODEL, yolo=True)

    result = await session.chat("run it")

    assert result.data["subtype"] == "success", result.data
    session.watch.assert_never("error")

    db_path = probe.env.wing_home / "core" / "wing.db"
    assert db_path.is_file(), f"wing.db 未创建：{db_path}"
    conn = read_only_connect(db_path)
    try:
        llm = conn.execute("SELECT * FROM audit_llm_call ORDER BY id").fetchall()
        tools = conn.execute("SELECT * FROM audit_tool_call ORDER BY id").fetchall()
    finally:
        conn.close()

    # 两次 LLM 调用各一行（tool 轮 + 收尾轮），字段逐列对账
    assert len(llm) == 2, [dict(row) for row in llm]
    assert [row["prompt_tokens"] for row in llm] == [100, 150]
    assert [row["completion_tokens"] for row in llm] == [20, 5]
    assert [row["cached_tokens"] for row in llm] == [40, 0]
    assert {row["model"] for row in llm} == {AUDIT_MODEL}
    assert {row["provider"] for row in llm} == {"probe"}
    assert {row["session_id"] for row in llm} == {session.session_id}
    assert all(row["ts_ms"] > 0 for row in llm)
    assert all(row["first_chunk_rt_ms"] > 0 for row in llm)
    assert all(row["tokens_per_sec"] > 0 for row in llm)
    assert all(row["duration_ms"] > 0 for row in llm)

    # 一次真实执行的工具调用恰好一行
    assert len(tools) == 1, [dict(row) for row in tools]
    tool = tools[0]
    assert tool["tool_name"] == "Bash"
    assert tool["tool_call_id"]
    assert tool["success"] == 1
    assert tool["session_id"] == session.session_id
    assert tool["duration_ms"] is not None and tool["duration_ms"] >= 0

    # 旧实现（每事件两次全量 JSON 重写）不再写
    assert not (probe.env.wing_home / "core" / "metrics.json").exists()


@pytest.mark.probe_env(models=[DEGRADED_MODEL], pre_start_dirs=["core/wing.db"])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_unopenable_database_does_not_break_the_gateway(probe: Probe) -> None:
    """``wing.db`` 打不开：审计降级、网关照常跑（一轮工具对话成功收口）。

    库路径在网关**启动前**就被占成一个目录（``pre_start_dirs``：进程启动期就会打开
    这个库——唤醒服务要拿它判"要不要武装"），于是 sqlite3 打不开；再跑一轮带工具调用的
    对话——审计是旁路，既不能拖垮 turn，也不能拖垮进程；降级只在打开时留一行 ERROR。
    """
    db_path = probe.env.wing_home / "core" / "wing.db"
    assert db_path.is_dir(), "库路径没有被提前占成目录"

    probe.register(
        DEGRADED_MODEL,
        Turn.of(tool_calls=[ToolCall("Bash", {"command": "echo still-alive"})]),
        Turn.of(text="survived"),
    )
    session = await probe.session(model=DEGRADED_MODEL, yolo=True)

    result = await session.chat("run it")

    assert result.data["subtype"] == "success", result.data
    session.watch.assert_never("error")
    # 库位置没被改写（仍是那个目录），也没有任何人替它建出文件
    assert db_path.is_dir()
    assert not (probe.env.wing_home / "core" / "wing.db-wal").exists()

    logs = sorted((probe.env.wing_home / "core" / "logs").glob("wing_*.log"))
    assert logs, "后端日志文件缺失"
    text = "\n".join(
        path.read_text(encoding="utf-8", errors="replace") for path in logs
    )
    assert "wing.db disabled" in text
