"""远程工具全链路场景：注册 → 调用 → 结果回灌 → 断连三面。

远程工具是"工具不必跑在 gateway 进程内"（AGENTS.md 不变量 4）的**唯一整机证据**：
`RemoteToolManager` 为每个注册工具构造绑定 client_id 的 dispatch 闭包注入核心 registry，
核心只看到一个普通 `Tool`——注册（HTTP + WS 归属校验）、调用（`tool_call_request` /
`tool_call_result` 往返）、断连（在途 fail + 全局注销 + 已绑定工具保护）任何一处改坏，
都只在跨进程真实调用时暴露。本文件用 probe 侧最小 tool host（`wing_probe.toolhost`）
走公开协议驱动，不 import wing。

两个用例：

- ``test_remote_tool_roundtrip_registers_and_returns_result``：注册响应与 `GET /api/tools`
  对账；模型调用远程工具 → `tool_call_result` 事件 + tool 消息进链；**下一轮请求体的工具
  schema 来自注册**（llm_name / description / params 逐字段）。
- ``test_remote_host_disconnect_aborts_in_flight_and_unregisters``：宿主在**在途调用**期间
  断开 → 该调用立即以 ``aborted`` 失败（不是等 ``remote_tool_timeout``）；工具从全局
  registry 注销；**已绑定该工具的会话不受影响**——继续对话仍成功、工具集声明逐字节不变
  （KV cache 保护不变量）、再次调用走 "client ... is not connected" 的立即失败路径。
  判别"立即失败 vs 超时兜底"用错误文案（`aborted` / `not connected` vs `timed out after`），
  墙钟只作宽松上界（默认 `remote_tool_timeout` = 1800s）。

协议事实来源：`libs/core/wing/gateway/protocol/system.py`（ToolCallRequest / ToolCallResult /
RemoteToolSpec）与 `docs/dev/http-api.md`「远程工具注册」。
"""

from __future__ import annotations

import asyncio
import time

import pytest

from wing_probe import Probe, ToolCall, ToolHost, Turn

ROUNDTRIP_MODEL = "probe/remote-tool-roundtrip"
DISCONNECT_MODEL = "probe/remote-tool-disconnect"

#: tool host 的 client_id（= 工具 namespace；`default` 是保留字）。
CLIENT_ID = "probe-host"

#: 注册的工具名 / 描述（描述唯一化，便于在请求体里精确对账）。
TOOL_NAME = "EchoNote"
TOOL_DESCRIPTION = "Echo a token back to the caller (probe remote tool)."

#: 注册声明的参数（下一轮请求体的 parameters 必须逐字段来自它）。
TOOL_TOKEN_PARAM = {
    "name": "token",
    "type": "string",
    "description": "token to echo",
}

#: `remote_tool_timeout` 的默认值（配置未暴露给 probe env）；墙钟上界取它的 1/60。
REMOTE_TOOL_TIMEOUT_S = 1800.0
IMMEDIATE_FAILURE_BUDGET_S = 30.0


async def _echo_note(token: str) -> str:
    return f"echo:{token}"


def _remote_refs(tools_payload: dict) -> list[dict]:
    return [tool for tool in tools_payload["tools"] if tool["namespace"] == CLIENT_ID]


def _tool_messages(probe: Probe, session_id: str) -> list[dict]:
    return [
        message
        for message in probe.history(session_id).messages()
        if message["role"] == "tool"
    ]


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_remote_tool_roundtrip_registers_and_returns_result(probe: Probe) -> None:
    """注册 → 模型调用 → 结果进链；请求体 schema 来自注册。"""
    host = ToolHost(CLIENT_ID, probe.env.gateway_url, started_at=probe.env.started_at)
    host.add_tool(
        TOOL_NAME,
        _echo_note,
        description=TOOL_DESCRIPTION,
        params=[TOOL_TOKEN_PARAM],
    )
    try:
        await host.start()
        assert host.registered == [f"{CLIENT_ID}.{TOOL_NAME}"], host.registered

        # 全局注册表视图：ref / namespace / llm_name / description 都是注册时的值。
        registry = await probe.driver_required.http.list_tools()
        remote = _remote_refs(registry)
        assert len(remote) == 1, registry
        assert remote[0]["ref"] == f"{CLIENT_ID}.{TOOL_NAME}", remote[0]
        assert remote[0]["name"] == TOOL_NAME, remote[0]
        assert remote[0]["llm_name"] == TOOL_NAME, remote[0]
        assert remote[0]["description"] == TOOL_DESCRIPTION, remote[0]

        probe.register(
            ROUNDTRIP_MODEL,
            Turn.of(tool_calls=[ToolCall(TOOL_NAME, {"token": "abc123"})]),
            Turn.of(text="done"),
        )
        session = await probe.session(
            model=ROUNDTRIP_MODEL, tools=[f"{CLIENT_ID}.{TOOL_NAME}"], yolo=True
        )

        result = await session.chat("please echo abc123")
        assert result.data["subtype"] == "success", result.data
        session.watch.assert_ordered(
            ["tool_call", "tool_call_result", "turn_result"], since=0
        )
        session.watch.assert_never("error")

        # 事件面：调用与结果；call_id 与 host 侧收到的帧一致。
        call_event = session.watch.events(type="tool_call")[0]
        result_event = session.watch.events(type="tool_call_result")[0]
        assert call_event.data["tool_name"] == TOOL_NAME, call_event.data
        assert call_event.data["tool_args"] == {"token": "abc123"}, call_event.data
        assert result_event.data["tool_result"] == "echo:abc123", result_event.data
        assert result_event.data["tool_success"] is True, result_event.data

        calls = host.calls_named(TOOL_NAME)
        assert len(calls) == 1, host.calls
        # 注意：帧上的 call_id 是 RemoteToolManager 为 pending future 生成的**独立**
        # id，不是模型侧 tool_call_id——两者不相等是协议事实（结果帧靠它寻址），
        # 归因靠 arguments 与"恰好一次"。
        assert calls[0].call_id != call_event.data["tool_call_id"], calls
        assert calls[0].arguments == {"token": "abc123"}, calls
        assert host.sent_results == [
            {
                "type": "tool_call_result",
                "call_id": calls[0].call_id,
                "result": "echo:abc123",
                "is_error": False,
            }
        ], host.sent_results

        # 落盘：tool 消息与 assistant 的 call_id 成对，结果与事件一致。
        history = probe.history(session)
        assert [message["role"] for message in history.messages()] == [
            "user",
            "assistant",
            "tool",
            "assistant",
        ], history.messages()
        tool_message = history.messages()[2]
        assert tool_message["tool_call_id"] == call_event.data["tool_call_id"]
        assert tool_message["content"] == "echo:abc123", tool_message
        history.assert_tool_pairing()

        # 请求面：下一轮声明的工具 schema **来自注册**（不是内置工具的残留）。
        follow_up = probe.context(ROUNDTRIP_MODEL, index=1)
        assert follow_up.tool_names == [TOOL_NAME], follow_up.describe()
        assert follow_up.messages[-1].role == "tool", follow_up.describe()
        body_tools = probe.request(ROUNDTRIP_MODEL, index=1).body["tools"]
        assert body_tools == [
            {
                "type": "function",
                "function": {
                    "name": TOOL_NAME,
                    "description": TOOL_DESCRIPTION,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "token": {"type": "string", "description": "token to echo"}
                        },
                        "required": ["token"],
                    },
                },
            }
        ], body_tools
        assert (await session.info())["tools"] == [TOOL_NAME]
    finally:
        await host.close()


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_remote_host_disconnect_aborts_in_flight_and_unregisters(
    probe: Probe,
) -> None:
    """断连：在途调用立即失败 + 全局注销；已绑定会话不受影响（KV cache 保护）。"""
    gate = asyncio.Event()

    async def hang(**_: object) -> str:
        """永不应答（除非场景放行）——在途调用的素材。"""
        await gate.wait()
        return "never"

    host = ToolHost(CLIENT_ID, probe.env.gateway_url, started_at=probe.env.started_at)
    host.add_tool(TOOL_NAME, hang, params=[TOOL_TOKEN_PARAM])
    try:
        await host.start()
        probe.register(
            DISCONNECT_MODEL,
            Turn.of(tool_calls=[ToolCall(TOOL_NAME, {"token": "hang"})]),
            Turn.of(text="recovered"),
            Turn.of(tool_calls=[ToolCall(TOOL_NAME, {"token": "again"})]),
            Turn.of(text="after second"),
        )
        session = await probe.session(
            model=DISCONNECT_MODEL, tools=[f"{CLIENT_ID}.{TOOL_NAME}"], yolo=True
        )

        # 第一轮：模型调用挂起工具 → 等到 host 真的收到请求（在途）→ 宿主断开。
        turn = asyncio.create_task(session.chat("hang please", within=90))
        call = await host.wait_for_call(TOOL_NAME, timeout=30)
        assert call.arguments == {"token": "hang"}, call

        started = time.monotonic()
        await host.close()
        result = await turn
        elapsed = time.monotonic() - started

        assert result.data["subtype"] == "success", result.data
        session.watch.assert_never("error")

        # 在途失败：文案是 fail_client 的 aborted 路径，**不是**超时兜底。
        first_result = session.watch.events(type="tool_call_result")[0]
        content = first_result.data["tool_result"]
        assert "aborted" in content, content
        assert "connection closed" in content, content
        assert "timed out" not in content, content
        assert first_result.data["tool_success"] is False, first_result.data
        # 二级旁证：立即失败远快于 remote_tool_timeout（1800s）——超时路径不可能
        # 在 30s 内完成，宽松上界只做"没有挂到超时"的常识校验。
        assert elapsed < IMMEDIATE_FAILURE_BUDGET_S, (
            f"disconnect→turn 收尾耗时 {elapsed:.1f}s，疑似走了超时兜底"
            f"（remote_tool_timeout 默认 {REMOTE_TOOL_TIMEOUT_S:.0f}s）"
        )
        assert _tool_messages(probe, session.session_id)[0]["content"] == content

        # 全局 registry：断连即注销该 namespace 的全部工具。
        registry = await probe.driver_required.http.list_tools()
        assert _remote_refs(registry) == [], registry

        # 第二轮：已绑定该工具的会话不受影响——对话仍成功，工具集声明逐字节不变
        # （KV cache 保护：fail_client 只清全局 registry，不动 agent 的 _tools）。
        assert probe.context(DISCONNECT_MODEL, index=1).tool_names == [TOOL_NAME]
        again = await session.chat("again please")
        assert again.data["subtype"] == "success", again.data

        second_result = session.watch.events(type="tool_call_result")[1]
        second_content = second_result.data["tool_result"]
        assert "is not connected" in second_content, second_content
        assert "unavailable" in second_content, second_content
        assert "timed out" not in second_content, second_content
        assert second_result.data["tool_success"] is False, second_result.data

        third_request = probe.request(DISCONNECT_MODEL, index=2)
        third_context = third_request.context()
        assert third_context.tool_names == [TOOL_NAME], third_context.describe()
        assert (
            third_request.body["tools"]
            == probe.request(DISCONNECT_MODEL, 0).body["tools"]
        ), "断连不得改动已绑定 agent 的工具集声明（KV cache 保护）"

        # host 侧只收到过第一次调用（断连后的调用在 gateway 侧就被拒了）。
        assert len(host.calls_named(TOOL_NAME)) == 1, host.calls
    finally:
        await host.close()
