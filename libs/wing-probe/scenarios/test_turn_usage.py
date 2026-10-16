"""turn 用量账与逐调用事件逐字段相等——收尾调用只记一次。

审计口径：一个 turn 的 usage = 该轮**每次 LLM 调用**的 usage 之和，与
``llm_call_metrics`` 直播事件逐字段同源（会话持久账本也由同一批事件累计，
「快照 + 增量」两端不得漂移）。曾有一次回归：收尾调用（无 tool call）被
``_call_llm_validated`` 与 ``_llm_turn`` 各记一笔——最后一笔 input / output /
cached 双计进 ``turn_result.usage``，与账本及直播事件对所有消费方（app 结算
缓存命中率、stdio result 帧）口径分裂。

覆盖断言点：

- ``test_turn_usage_matches_per_call_events``：工具轮 + 收尾轮（usage 互不
  相同）——``turn_result.usage`` 逐字段等于两次 ``llm_call_metrics`` 之和，
  与盘上账本一致（双计会把和抬成"多记一笔收尾"）；
- ``test_retried_attempt_usage_counted_once``：空的无效尝试（带 usage）被
  丢弃后重试成功——被丢弃尝试的 usage 计入一次、收尾一次：turn 账、逐
  调用事件与盘上账本三方一致（不丢、不重）。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, ToolCall, Turn, Usage

MAIN_MODEL = "probe/turn-usage-main"
RETRY_MODEL = "probe/turn-usage-retry"

#: 两次调用的 usage 互不相同——双计会改变和，逐字段定位到哪一笔被重复。
TOOL_TURN = Usage(prompt_tokens=100, completion_tokens=20, cached_tokens=40)
FINAL_TURN = Usage(prompt_tokens=30, completion_tokens=5, cached_tokens=10)


def _sum_metrics(events: list) -> tuple[int, int, int]:
    """Σ（prompt / completion / cached）——与 turn 账、会话账本同口径。"""
    return (
        sum(e.data["prompt_tokens"] for e in events),
        sum(e.data["completion_tokens"] for e in events),
        sum(e.data["cached_tokens"] for e in events),
    )


def _turn_tokens(result_usage: dict) -> tuple[int, int, int]:
    return (
        result_usage["input_tokens"],
        result_usage["output_tokens"],
        result_usage["cached_tokens"],
    )


@pytest.mark.probe_env(models=[MAIN_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_turn_usage_matches_per_call_events(probe: Probe) -> None:
    """工具轮 + 收尾轮：turn_result.usage == Σ llm_call_metrics == 盘上账本。"""
    probe.register(
        MAIN_MODEL,
        Turn.of(
            tool_calls=[ToolCall("Bash", {"command": "sleep 0.02"})], usage=TOOL_TURN
        ),
        Turn.of(text="done", usage=FINAL_TURN),
    )
    session = await probe.session(model=MAIN_MODEL, yolo=True)

    result = await session.chat("go")

    assert result.data["subtype"] == "success", result.data
    session.watch.assert_never("error")

    metrics = session.watch.events("llm_call_metrics")
    assert len(metrics) == 2, [e.type for e in session.timeline.all()]
    summed = _sum_metrics(metrics)
    assert summed == (
        TOOL_TURN.prompt_tokens + FINAL_TURN.prompt_tokens,
        TOOL_TURN.completion_tokens + FINAL_TURN.completion_tokens,
        TOOL_TURN.cached_tokens + FINAL_TURN.cached_tokens,
    ), [e.data for e in metrics]

    assert _turn_tokens(result.data["usage"]) == summed, result.data["usage"]

    # 盘上账本与逐调用事件同源：turn 账与它逐字段相等（口径唯一）。
    recorded = probe.history(session).metadata()
    assert recorded is not None, "metadata.json missing"
    ledger = recorded["usage"]
    assert (
        ledger["prompt_tokens"],
        ledger["completion_tokens"],
        ledger["cached_tokens"],
    ) == summed, ledger


@pytest.mark.probe_env(models=[RETRY_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_retried_attempt_usage_counted_once(probe: Probe) -> None:
    """无效尝试被丢弃但 token 真花掉：它计一次、收尾计一次——和不变。"""
    retry = Usage(prompt_tokens=40, completion_tokens=6, cached_tokens=4)
    final = Usage(prompt_tokens=12, completion_tokens=3, cached_tokens=2)
    probe.register(
        RETRY_MODEL,
        Turn.of(usage=retry),  # ① 限流式空响应（有 usage）→ 无效 → 重试
        Turn.of(text="recovered", usage=final),  # ② 收尾
    )
    session = await probe.session(model=RETRY_MODEL)

    result = await session.chat("go")

    assert result.data["subtype"] == "success", result.data
    session.watch.assert_ordered(["turn_started", "notice", "turn_result"], since=0)
    session.watch.assert_never("error")

    metrics = session.watch.events("llm_call_metrics")
    assert len(metrics) == 2, [e.type for e in session.timeline.all()]
    summed = _sum_metrics(metrics)
    assert summed == (
        retry.prompt_tokens + final.prompt_tokens,
        retry.completion_tokens + final.completion_tokens,
        retry.cached_tokens + final.cached_tokens,
    ), [e.data for e in metrics]

    assert _turn_tokens(result.data["usage"]) == summed, result.data["usage"]

    # 盘上账本与逐调用事件同源：被丢弃尝试的 token 真花掉了，必须一并入账。
    recorded = probe.history(session).metadata()
    assert recorded is not None, "metadata.json missing"
    ledger = recorded["usage"]
    assert (
        ledger["prompt_tokens"],
        ledger["completion_tokens"],
        ledger["cached_tokens"],
    ) == summed, ledger
