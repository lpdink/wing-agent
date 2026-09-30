"""LLM 调用指标场景：``llm_call_metrics`` 的 decode TPS。

覆盖的断言点：

- ``test_tool_call_only_turn_reports_decode_tps``：模型只回 tool call
  （无正文 / 无思考）的一轮——``llm_call_metrics`` 的 ``completion_tokens``
  等于剧本 usage，且 ``tokens_per_sec`` 为正（首 token 打点覆盖 tool 参数
  增量；回归：唯一打点源缺失导致恒为 0，前端按 ``>0`` 展示时看不到 TPS）；
  随后的正文轮（对照组）TPS 同样为正。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, ToolCall, Turn, Usage

#: 场景私有 model 名（design D3：剧本按 model 名路由，场景之间不共享）。
TPS_MODEL = "probe/llm-metrics-tps"


@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_tool_call_only_turn_reports_decode_tps(probe: Probe) -> None:
    """纯 tool call 轮的 llm_call_metrics 必须携带正数 decode TPS。

    WHEN 剧本第一轮只回 tool call（无正文 / 无思考、带 usage），第二轮回正文
    THEN 两轮的 ``llm_call_metrics`` 都到达；第一轮（tool-call-only）的
    ``tokens_per_sec`` 为正数——首 token 打点覆盖了 tool 参数增量。
    """
    probe.register(
        TPS_MODEL,
        Turn.of(
            tool_calls=[ToolCall("Bash", {"command": "echo probe-tps"})],
            usage=Usage(prompt_tokens=100, completion_tokens=30),
            # 帧间延迟：让"首 token → usage"的 decode 窗口可测（非零）。
            delay=0.02,
        ),
        Turn.of(text="done", usage=Usage(prompt_tokens=150, completion_tokens=5)),
    )
    # yolo：Bash 免确认（本场景断言的是指标事件，不是安全审查）。
    session = await probe.session(model=TPS_MODEL, yolo=True)

    result = await session.chat("run it")

    assert result.data["subtype"] == "success", result.data
    session.watch.assert_never("error")

    metrics = session.watch.events("llm_call_metrics")
    assert len(metrics) == 2, [e.type for e in session.timeline.all()]

    tool_only, text_turn = metrics
    assert tool_only.data["completion_tokens"] == 30, tool_only.data
    assert tool_only.data["tokens_per_sec"] > 0, tool_only.data
    assert text_turn.data["completion_tokens"] == 5, text_turn.data
    assert text_turn.data["tokens_per_sec"] > 0, text_turn.data
