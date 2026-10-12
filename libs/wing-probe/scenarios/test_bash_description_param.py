"""Bash 工具 schema 的参数名（Claude Code 兼容：``description``）。

两个断言点：

- **声明面**：模型看到的 Bash schema 暴露**可选** ``description`` 参数（模型 SFT
  数据里惯用这个字段名，部分第三方集成也只认它），旧的 ``purpose`` 不再出现；
- **容忍面**：附加参数不进函数签名，执行器按签名过滤消化——带 ``description``
  （以及模型仍按旧习惯吐出的 ``purpose``）的调用照常执行，改名不制造新失败。

断言面：假 Provider 留档的请求体（``tools`` 里的 Bash schema）、事件、workspace
文件、落盘链。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, ToolCall, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
DESCRIBED_MODEL = "probe/bash-description-arg"
LEGACY_MODEL = "probe/bash-legacy-purpose-arg"

#: workspace 证据：命令真的跑过。
DESCRIBED_FILE = "described.txt"
LEGACY_FILE = "legacy-purpose.txt"


def _bash_function_schema(probe: Probe, model: str) -> dict:
    """第一次 LLM 请求体里 Bash 的函数 schema（OpenAI 线格式）。"""
    tools = probe.request(model, 0).body["tools"]
    for tool in tools:
        if tool["function"]["name"] == "Bash":
            return tool["function"]
    declared = [tool["function"]["name"] for tool in tools]
    raise AssertionError(f"Bash not declared; got {declared}")


@pytest.mark.probe_env(models=[DESCRIBED_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_bash_schema_exposes_description_param(probe: Probe) -> None:
    """Bash schema 带可选 ``description``、不带 ``purpose``（红线）。

    WHEN yolo 会话里剧本带 ``description`` 发起一次 Bash 调用
    THEN 请求体里 Bash 的 properties 含 ``description``（string）且不含
    ``purpose``、``description`` 不在 required 里；调用照常执行（文件效果）、
    事件与落盘链自洽、无 error。
    """
    probe.register(
        DESCRIBED_MODEL,
        Turn.of(
            tool_calls=[
                ToolCall(
                    "Bash",
                    {
                        "command": f"printf ok > {DESCRIBED_FILE}",
                        "description": "写一个探针文件",
                    },
                )
            ]
        ),
        Turn.of(text="done"),
    )
    session = await probe.session(model=DESCRIBED_MODEL, yolo=True)

    result = await session.chat("write the file")
    assert result.data["subtype"] == "success", result.data

    parameters = _bash_function_schema(probe, DESCRIBED_MODEL)["parameters"]
    properties = parameters["properties"]
    assert properties["description"]["type"] == "string", properties
    assert "purpose" not in properties, properties
    assert "description" not in parameters["required"], parameters
    assert "command" in parameters["required"], parameters

    tool_call = session.watch.events("tool_call")[0]
    assert tool_call.data["tool_args"]["description"] == "写一个探针文件", (
        tool_call.data
    )
    tool_result = session.watch.events("tool_call_result")[0]
    assert tool_result.data["tool_success"] is True, tool_result.data
    session.watch.assert_never("error")
    probe.files.assert_content(DESCRIBED_FILE, equals="ok")

    session.history.assert_tool_pairing()


@pytest.mark.probe_env(models=[LEGACY_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_bash_call_with_legacy_purpose_arg_still_executes(probe: Probe) -> None:
    """沿用旧字段名 ``purpose`` 的调用不因改名炸掉（容忍契约）。

    WHEN 剧本按改名前的习惯发一次带 ``purpose`` 的 Bash 调用
    THEN 调用照常执行（附加参数被签名过滤消化）、无 error、落盘链完整。
    """
    probe.register(
        LEGACY_MODEL,
        Turn.of(
            tool_calls=[
                ToolCall(
                    "Bash",
                    {"command": f"printf ok > {LEGACY_FILE}", "purpose": "旧字段名"},
                )
            ]
        ),
        Turn.of(text="done"),
    )
    session = await probe.session(model=LEGACY_MODEL, yolo=True)

    result = await session.chat("write the file")
    assert result.data["subtype"] == "success", result.data

    tool_result = session.watch.events("tool_call_result")[0]
    assert tool_result.data["tool_success"] is True, tool_result.data
    session.watch.assert_never("error")
    probe.files.assert_content(LEGACY_FILE, equals="ok")

    session.history.assert_tool_pairing()
