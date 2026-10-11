"""LLM 调用指标场景：``llm_call_metrics.first_chunk_rt_ms`` 的 ttft 口径。

覆盖的断言点：

- ``test_ttft_measures_first_content_frame``：慢轮（``first_delay=0.6``，
  即"响应头 / 空 role 帧已到、首个内容帧尚未到"的窗口）的
  ``first_chunk_rt_ms`` 必须反映该窗口；同一会话的快轮显著更小。

回归背景：该字段曾以"响应头到达"打点——对"先开流、后生成"的服务端（立即回
200 + 空 role 帧，排队 / 预填充后才吐第一个 token）会报出十几毫秒，与用户
真实等待（秒级）脱节。修复后打点落在首个内容帧。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, Turn, Usage

#: 场景私有 model 名（design D3：剧本按 model 名路由，场景之间不共享）。
TTFT_MODEL = "probe/llm-metrics-ttft"

#: 慢轮的首个内容帧等待（秒）——"turn 已开始、首个内容块尚未到达"的窗口。
FIRST_DELAY = 0.6


@pytest.mark.probe_env(models=[TTFT_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_ttft_measures_first_content_frame(probe: Probe) -> None:
    """慢轮 ttft 反映首帧窗口，而不是响应头时刻。

    WHEN 会话第一轮流式响应把首个内容帧推迟 ``FIRST_DELAY`` 秒，第二轮
    不带延迟
    THEN 两轮的 ``llm_call_metrics`` 都到达；慢轮的 ``first_chunk_rt_ms``
    不小于窗口的大半，快轮显著更小——口径落在首个内容帧上。
    """
    probe.register(
        TTFT_MODEL,
        Turn.of(
            text="slow",
            usage=Usage(prompt_tokens=100, completion_tokens=2),
            first_delay=FIRST_DELAY,
        ),
        Turn.of(
            text="fast",
            usage=Usage(prompt_tokens=120, completion_tokens=2),
        ),
    )
    session = await probe.session(model=TTFT_MODEL)

    await session.chat("slow one")
    await session.chat("fast one")
    session.watch.assert_never("error")

    metrics = session.watch.events("llm_call_metrics")
    assert len(metrics) == 2, [e.type for e in session.timeline.all()]

    slow, fast = metrics
    assert slow.data["completion_tokens"] == 2, slow.data
    assert slow.data["first_chunk_rt_ms"] >= FIRST_DELAY * 1000 * 0.75, slow.data
    assert fast.data["first_chunk_rt_ms"] < slow.data["first_chunk_rt_ms"], (
        slow.data,
        fast.data,
    )
