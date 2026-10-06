"""Bash 子进程环境变量场景（工具链路面 · `WING_SESSION_ID` 注入）。

被守的语义（`tools/builtin/bash.py::_execute_command`）：

- Bash 工具启动的 shell **显式**收到 `WING_SESSION_ID=<本会话 id>`——不依赖网关
  进程环境继承（继承可能是旧值 / 错值 / 缺失）；
- 值是该**执行工具调用的会话**的真实 id（即创建响应里的
  `session.session_id`），不是常量、不是空串、不是别的会话。

断言面：工具结果（`printf '%s' "$WING_SESSION_ID"` 的 stdout 逐字等于会话真实
id）、事件（tool_call 参数与它一致、无 error）、落盘链（tool 配对完整）。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, ToolCall, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
ENV_MODEL = "probe/bash-session-env"

#: 打印环境变量原文（无换行、无其它输出——结果行可与会话 id 逐字比对）。
COMMAND = "printf '%s' \"$WING_SESSION_ID\""


#: 把网关进程 env 污染成"带旧值"——显式覆盖语义由此变成端到端确定性验证：
#: 没有注入时命令会漏出 stale-outer-session，注入后必须是真实会话 id。
@pytest.mark.probe_env(env_overrides={"WING_SESSION_ID": "stale-outer-session"})
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_bash_sees_own_session_id_env(probe: Probe) -> None:
    """Bash 工具里 `WING_SESSION_ID` == 本会话真实 id（红线）。

    网关进程 env 预置了旧值 `stale-outer-session`（probe_env 旋钮）——"显式
    覆盖、不依赖继承"因此成为场景的确定断言，而非"恰好没触发"。

    WHEN yolo 会话里剧本发起一次 Bash 调用（打印该环境变量）
    THEN 工具结果里 stdout 逐字等于该会话的真实 session id；调用参数、事件与
    落盘链一致，且没有任何 error 收场。
    """
    probe.register(
        ENV_MODEL,
        Turn.of(tool_calls=[ToolCall("Bash", {"command": COMMAND})]),
        Turn.of(text="checked"),
    )
    session = await probe.session(model=ENV_MODEL, yolo=True)
    assert session.session_id, session.response

    result = await session.chat("print the session id")
    assert result.data["subtype"] == "success", result.data

    tool_call = session.watch.events("tool_call")[0]
    assert tool_call.data["tool_name"] == "Bash", tool_call.data
    assert tool_call.data["tool_args"]["command"] == COMMAND, tool_call.data

    tool_result = session.watch.events("tool_call_result")[0]
    assert tool_result.data["tool_call_id"] == tool_call.data["tool_call_id"], (
        tool_result.data
    )
    assert tool_result.data["tool_success"] is True, tool_result.data
    # `_format_result` 首行是 `[exit code: N | Xs]`，其后即命令 stdout 原文。
    lines = tool_result.data["tool_result"].split("\n", 1)
    assert len(lines) == 2 and lines[1] == session.session_id, tool_result.data

    session.watch.assert_never("error")

    view = session.history
    view.assert_tool_pairing()
    view.assert_chain_invariants()
