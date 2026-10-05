"""非法 tool args 短路场景（工具链路面 · 覆盖点 1）。

被守的语义（`provider/base.py::parse_tool_args` + `agent/tool_executor.py`）：

- 模型吐出的 args JSON 可能非法（尾逗号 / 未转义引号 / 非 object）。**解析失败绝不抛异常**：
  抛了会触发整轮的无效重试，把本轮已生成的 thinking / content / tool call 一起丢掉；
- 正确路径是**短路执行**：`ToolCall.arguments_error` 非空 → 工具函数**不调用**，把"未执行"
  信封作为工具结果回灌给模型自纠（信封里逐字带上模型吐的原始 args —— 自纠的输入就是它）。

断言面：工具结果文本（信封逐字）、**剧本消费次数**（没有整轮重试的结构性证据）、
下一轮请求体（模型拿到的错误上下文）、workspace 文件（命令没跑过）、落盘链（配对完整）。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, ToolCall, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
MALFORMED_MODEL = "probe/malformed-args"
NON_OBJECT_MODEL = "probe/malformed-args-shape"

#: 模型吐出的非法 args（尾逗号）——`ToolCall.argument_text` 原样透传，provider 容错解析失败。
REJECTED_FILE = "rejected.txt"
MALFORMED_ARGS = f'{{"command": "printf corrupt > {REJECTED_FILE}",}}'

#: 自纠后模型重发的合法参数（写另一个文件：两次调用的副作用互不遮挡）。
RECOVERED_FILE = "recovered.txt"
RECOVERED_COMMAND = f"printf recovered > {RECOVERED_FILE}"

#: 信封锚点（`ToolExecutor._execute_one` 的短路分支 + `parse_tool_args` 的错误文本）。
SKIP_ANCHOR = "was NOT executed"
ARGS_ECHO_ANCHOR = "Arguments received:"


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_malformed_args_short_circuit_then_self_correct(probe: Probe) -> None:
    """非法 args：不执行 + 不重试 + 错误回灌 → 下一轮自纠成功（红线）。

    WHEN 剧本第一轮吐出非法 JSON 的 ``Bash`` 调用（尾逗号）
    THEN 该轮**不重试**（剧本只有三轮：非法调用 / 合法调用 / 收尾文本——多一次请求就会
    耗尽剧本吃 5xx，`chat()` 直接失败）、工具函数**没被调用**（命令本该写的文件不存在）、
    工具结果是"未执行"信封且逐字回带模型吐的原始 args；模型据此重发合法调用 → 真的执行
    成功（workspace 文件为证），assistant/tool 配对完整、错误进了下一轮请求上下文。
    """
    probe.register(
        MALFORMED_MODEL,
        Turn.of(tool_calls=[ToolCall("Bash", MALFORMED_ARGS)]),
        Turn.of(tool_calls=[ToolCall("Bash", {"command": RECOVERED_COMMAND})]),
        Turn.of(text="recovered"),
    )
    probe.allow_arguments_error(
        "非法 args 场景：模型有意吐出无法解析的参数，断言面就是短路回灌本身"
    )
    session = await probe.session(model=MALFORMED_MODEL, yolo=True)

    result = await session.chat("try to write the file")
    assert result.data["subtype"] == "success", result.data
    session.watch.assert_never("error")

    # ── 没有整轮重试：三个剧本 Turn 恰好对应三次请求（多一次即 5xx，见 docstring） ──
    assert probe.requests.count(MALFORMED_MODEL) == 3, probe.requests.summary()

    # ── 未执行：信封 + 逐字回带原始 args + 零副作用 ──
    view = probe.history(session)
    tool_messages = [
        message for message in view.messages() if message["role"] == "tool"
    ]
    assert len(tool_messages) == 2, view.describe()
    rejected = tool_messages[0]
    assert SKIP_ANCHOR in rejected["content"], rejected
    assert MALFORMED_ARGS in rejected["content"], rejected
    assert ARGS_ECHO_ANCHOR in rejected["content"], rejected
    assert (
        "Please re-issue the tool call with corrected arguments."
        in (rejected["content"])
    ), rejected
    probe.files.assert_missing(REJECTED_FILE)

    # ── 自纠成功：第二个调用真的执行 ──
    accepted = tool_messages[1]
    assert SKIP_ANCHOR not in accepted["content"], accepted
    probe.files.assert_content(RECOVERED_FILE, equals="recovered")

    # ── 配对完整 + 模型下一轮真的拿到了错误（错误进了请求上下文） ──
    view.assert_tool_pairing(allow_arguments_error=True)
    view.assert_chain_invariants()
    assert [message["role"] for message in view.messages()] == [
        "user",
        "assistant",
        "tool",
        "assistant",
        "tool",
        "assistant",
    ], view.describe()

    follow_up = probe.context(MALFORMED_MODEL, 1)
    # 请求体里 assistant 侧的裸 input `{}` 无法证明参数合法（原文在 error 信封里），
    # 所以只对账配对结构。
    follow_up.assert_tool_pairing(require_json_args=False)
    replayed = [message for message in follow_up.messages if message.role == "tool"]
    assert replayed[0].content == rejected["content"], follow_up.describe()
    assert MALFORMED_ARGS in replayed[0].content, follow_up.describe()

    # ── 广播面：前端拿到的第一条结果是失败信封（`tool_success=False`） ──
    session.watch.assert_ordered(
        ["tool_call", "tool_call_result", "tool_call", "tool_call_result"],
        since=0,
    )
    results = session.watch.events(type="tool_call_result")
    assert [event.data["tool_success"] for event in results] == [False, True], [
        event.data for event in results
    ]
    assert SKIP_ANCHOR in results[0].data["tool_result"], results[0].data


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_non_object_args_are_rejected_without_execution(probe: Probe) -> None:
    """非 object 的 args（合法 JSON 但不是对象）走同一条短路路径。

    WHEN 剧本吐出 ``["ls"]``（合法 JSON 数组，模型偶发的形态）
    THEN 工具未执行、信封说明"must be a JSON object"并回带原文——解析容错的两条分支
    （JSON 语法错 / 形状错）都被整机覆盖。
    """
    probe.register(
        NON_OBJECT_MODEL,
        Turn.of(tool_calls=[ToolCall("Bash", '["ls"]')]),
        Turn.of(text="noted"),
    )
    probe.allow_arguments_error(
        "非法 args 场景：模型有意吐出非 object 的参数（形状错分支）"
    )
    session = await probe.session(model=NON_OBJECT_MODEL, yolo=True)

    result = await session.chat("list something")
    assert result.data["subtype"] == "success", result.data
    assert probe.requests.count(NON_OBJECT_MODEL) == 2, probe.requests.summary()

    view = probe.history(session)
    tool_messages = [
        message for message in view.messages() if message["role"] == "tool"
    ]
    assert len(tool_messages) == 1, view.describe()
    assert "must be a JSON object" in tool_messages[0]["content"], tool_messages[0]
    assert '["ls"]' in tool_messages[0]["content"], tool_messages[0]
    view.assert_tool_pairing(allow_arguments_error=True)
    session.watch.assert_never("error")
