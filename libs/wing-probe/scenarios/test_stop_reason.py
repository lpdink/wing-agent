"""stop_reason 捕获场景（截断审计，#151）。

被守的语义（`provider/openai/*` + `agent/react_loop.py::_call_llm`）：

- OpenAI 兼容**流式**主路径：`finish_reason=length`（max_tokens 截断）必须传导进
  assistant Message 的 `stop_reason` 并落盘——与 Anthropic 的 `max_tokens` /
  非流式路径同语义。带内 usage chunk（非零 token、喂 metrics）与流尾终帧
  （零 token、喂 Message 组装）各持一半信息，消费侧不许按"有没有 token"取舍；
- 对照组：`finish_reason=stop` 的正常收尾；
- 直播路径：`llm_call_metrics`（带内 usage 帧的载荷）与 `assistant_turn`
  （stdio / ACP 消费者看到的 turn 级事件）同样携带真实值——后者按 Claude 词表
  翻译（`length` → `max_tokens`），编排器据此区分截断与正常收尾。

断言面：事件时间线（llm_call_metrics.stop_reason）、落盘链（assistant Message 的
stop_reason / usage；内置不变量照常）。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, Turn, Usage

#: 场景私有 model 名（剧本按 model 名路由，场景之间零共享）。
STOP_REASON_MODEL = "probe/stop-reason"

#: 第一轮：上游 max_tokens 截断（finish_reason=length）。
TRUNCATED_TEXT = "the answer is cut o"
TRUNCATED_USAGE = Usage(prompt_tokens=100, completion_tokens=7)

#: 第二轮：正常收尾（finish_reason=stop，剧本缺省）。
NORMAL_TEXT = "all done"
NORMAL_USAGE = Usage(prompt_tokens=120, completion_tokens=3)


@pytest.mark.probe_env(models=[STOP_REASON_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_streaming_stop_reason_survives_to_disk(probe: Probe) -> None:
    """流式 length / stop：Message 与落盘、直播 metrics 三处同值。

    WHEN 剧本第一轮以 ``finish_reason=length`` 收尾、第二轮回正常 ``stop``
    THEN 两轮的 ``llm_call_metrics.stop_reason`` 分别为 ``length`` / ``stop``；
    落盘 history 的 assistant 记录同样逐条携带（截断审计可读），usage 保持
    带内非零值（终帧零 token 不覆盖）。
    """
    probe.register(
        STOP_REASON_MODEL,
        Turn.of(text=TRUNCATED_TEXT, usage=TRUNCATED_USAGE, finish="length"),
        Turn.of(text=NORMAL_TEXT, usage=NORMAL_USAGE),
    )
    session = await probe.session(model=STOP_REASON_MODEL)

    first = await session.chat("say something long")
    assert first.data["subtype"] == "success", first.data
    second = await session.chat("and now finish normally")
    assert second.data["subtype"] == "success", second.data
    session.watch.assert_never("error")

    # ── 直播：带内 usage 帧（metrics 事件）携带 stop_reason ──
    metrics = session.watch.events("llm_call_metrics")
    assert [event.data.get("stop_reason") for event in metrics] == [
        "length",
        "stop",
    ], [event.data for event in metrics]
    assert [event.data["completion_tokens"] for event in metrics] == [
        TRUNCATED_USAGE.completion_tokens,
        NORMAL_USAGE.completion_tokens,
    ], [event.data for event in metrics]

    # ── 直播：turn 级事件（stdio / ACP）报 Claude 词表的真实值 ──
    turns = session.watch.events("assistant_turn")
    assert [event.data.get("stop_reason") for event in turns] == [
        "max_tokens",  # OpenAI 的 length 翻译成 Claude 词表
        "end_turn",
    ], [event.data for event in turns]

    # ── 落盘：assistant Message 的 stop_reason 与 usage 逐条对账 ──
    view = probe.history(session)
    view.assert_chain_invariants()
    assistants = [
        message for message in view.messages() if message["role"] == "assistant"
    ]
    assert [message.get("stop_reason") for message in assistants] == [
        "length",
        "stop",
    ], view.describe()
    assert [message["usage"]["completion_tokens"] for message in assistants] == [
        TRUNCATED_USAGE.completion_tokens,
        NORMAL_USAGE.completion_tokens,
    ], view.describe()
