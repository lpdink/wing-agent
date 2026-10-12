"""会话用量 / 运行时长聚合——落盘、API 与跨 release / 重启延续。

账本口径（`SessionUsage`）：计数（user_messages / llm_calls / tool_calls /
turns）+ token（prompt / completion / cached）+ 时长（llm_time_ms /
tool_time_ms）。本场景锚定四个面：

- 数值正确：假 Provider 剧本给定 usage，API 与盘上逐字段相等；
- 落盘：``metadata.json.usage`` 与 API 同值（回合收尾即落）；
- release + resume：``/api/session/release`` 逐出后水合，累计从盘上延续；
- 网关重启：``restart_gateway`` 后同一 session id 继续累计（跨进程延续）。

命令时长不做精确断言（调度抖动），只断言正数（工具用 ``sleep`` 撑出可观测
时长）——响应里没有第三个「工作时长」字段：工作时长 = llm + tool 由客户端
推导，账本不存派生值。
"""

from __future__ import annotations

import pytest

from wing_probe import Probe, ToolCall, Turn
from wing_probe.driver.session import Driver
from wing_probe.provider.script import Usage

MODEL = "probe/session-usage"

#: 一轮对话的剧本：一次带工具调用的 LLM 轮 + 一次收尾轮（usage 互不相同）。
TOOL_TURN = Usage(prompt_tokens=100, completion_tokens=20, cached_tokens=40)
FINAL_TURN = Usage(prompt_tokens=30, completion_tokens=5, cached_tokens=10)

#: 一轮对话对账本的贡献（= 两轮 LLM 调用之和）。
ROUND_TOKENS = (
    TOOL_TURN.prompt_tokens + FINAL_TURN.prompt_tokens,
    TOOL_TURN.completion_tokens + FINAL_TURN.completion_tokens,
    TOOL_TURN.cached_tokens + FINAL_TURN.cached_tokens,
)

COUNTS = (
    "user_messages",
    "llm_calls",
    "tool_calls",
    "turns",
    "prompt_tokens",
    "completion_tokens",
    "cached_tokens",
    "llm_time_ms",
    "tool_time_ms",
)


def _driver(probe: Probe) -> Driver:
    return probe.driver_required


async def _usage(probe: Probe, session_id: str) -> dict:
    """``GET /api/session/info`` 的 usage 段。"""
    payload = await _driver(probe).http.request(
        "GET", "/api/session/info", params={"session_id": session_id}
    )
    usage = payload.get("usage")
    assert isinstance(usage, dict), f"usage missing from /api/session/info: {payload}"
    return usage


def _assert_usage(
    usage: dict | None,
    *,
    messages: int,
    llm_calls: int,
    tool_calls: int,
    turns: int,
    tokens: tuple[int, int, int],
) -> None:
    """逐字段期望（时长只断言正数：真实测量、不发明常量）。"""
    assert usage is not None, "usage record missing"
    assert set(usage) == set(COUNTS), usage
    assert usage["user_messages"] == messages, usage
    assert usage["llm_calls"] == llm_calls, usage
    assert usage["tool_calls"] == tool_calls, usage
    assert usage["turns"] == turns, usage
    assert (
        usage["prompt_tokens"],
        usage["completion_tokens"],
        usage["cached_tokens"],
    ) == tokens, usage
    assert usage["llm_time_ms"] > 0, usage
    assert usage["tool_time_ms"] > 0, usage


def _tokens(rounds: int) -> tuple[int, int, int]:
    """``rounds`` 轮对话后的 token 三元组（prompt / completion / cached）。"""
    return (
        ROUND_TOKENS[0] * rounds,
        ROUND_TOKENS[1] * rounds,
        ROUND_TOKENS[2] * rounds,
    )


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_usage_accumulates_persists_and_survives_release_and_restart(
    probe: Probe,
) -> None:
    """一段真实会话里的用量：API 可读、落 metadata.json、release + 重启后延续。"""
    probe.register(
        MODEL,
        # 三轮对话 × （工具轮 + 收尾轮）：release 与网关重启各切断一次。
        *[
            turn
            for _ in range(3)
            for turn in (
                Turn.of(
                    tool_calls=[ToolCall("Bash", {"command": "sleep 0.05"})],
                    usage=TOOL_TURN,
                ),
                Turn.of(text="done", usage=FINAL_TURN),
            )
        ],
    )
    session = await probe.session(model=MODEL, yolo=True)
    result = await session.chat("first round")
    assert result.data["subtype"] == "success", result.data

    first = await _usage(probe, session.session_id)
    _assert_usage(
        first, messages=1, llm_calls=2, tool_calls=1, turns=1, tokens=_tokens(1)
    )
    # 回合收尾已落盘：盘上记录与 API 同一份数字。
    recorded = probe.history(session).metadata()
    assert recorded is not None and recorded.get("usage") == first

    # release（先退订——订阅中的会话被钉住）→ 重新订阅即水合，累计不丢。
    await _driver(probe).http.unsubscribe(session.session_id, _driver(probe).client_id)
    released = await _driver(probe).http.release_session(session.session_id)
    assert released.get("released") is True, released

    await _driver(probe).http.subscribe(session.session_id, _driver(probe).client_id)
    assert await _usage(probe, session.session_id) == first
    result = await session.chat("second round")
    assert result.data["subtype"] == "success", result.data

    second = await _usage(probe, session.session_id)
    _assert_usage(
        second, messages=2, llm_calls=4, tool_calls=2, turns=2, tokens=_tokens(2)
    )

    # 网关重启（进程消亡）后同一会话继续累计——账本在磁盘上，不在内存里。
    await probe.restart_gateway()
    resumed_again = await probe.resume(session.session_id)
    assert await _usage(probe, session.session_id) == second
    result = await resumed_again.chat("third round")
    assert result.data["subtype"] == "success", result.data

    third = await _usage(probe, session.session_id)
    _assert_usage(
        third, messages=3, llm_calls=6, tool_calls=3, turns=3, tokens=_tokens(3)
    )
    recorded = probe.history(session).metadata()
    assert recorded is not None and recorded.get("usage") == third


#: 非法参数（尾逗号）：工具执行器短路回灌，不执行、不计一次调用。
MALFORMED_ARGS = '{"command": "printf nope",}'


@pytest.mark.probe_env(models=[MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_usage_starts_at_zero_and_skips_unexecuted_tool_calls(
    probe: Probe,
) -> None:
    """未产生用量的会话读零；没真正执行的工具调用（短路 / 中断合成）都不计。"""
    probe.register(
        MODEL,
        # ① 非法 args 的 Bash 调用：短路回灌（不执行）。
        Turn.of(tool_calls=[ToolCall("Bash", MALFORMED_ARGS)]),
        # ② 自纠收尾：本轮正常结束（turn 收尾计入）。
        Turn.of(text="recovered"),
        # ③ 慢分片：给中断留出窗口（tool_call 之前）。
        Turn.of(text="thinking out loud", chunk=1, delay=0.5),
        Turn.of(text="never reached"),
    )
    probe.allow_arguments_error(
        "usage 场景：有意让模型吐出坏参数——断言面是「短路没有真正执行」不进账本"
    )
    session = await probe.session(model=MODEL, yolo=True)

    zeros = await _usage(probe, session.session_id)
    assert zeros == dict.fromkeys(COUNTS, 0), zeros

    result = await session.chat("try a broken tool call")
    assert result.data["subtype"] == "success", result.data

    after_short_circuit = await _usage(probe, session.session_id)
    # 一轮完整对话：用户消息 1、收尾 1；坏参数的调用没有执行，不计调用与时长。
    assert after_short_circuit["user_messages"] == 1, after_short_circuit
    assert after_short_circuit["turns"] == 1, after_short_circuit
    assert after_short_circuit["tool_calls"] == 0, after_short_circuit
    assert after_short_circuit["tool_time_ms"] == 0, after_short_circuit
    # 剧本没给 usage：token / llm 调用也不计（账本不发明数字）。
    assert after_short_circuit["llm_calls"] == 0, after_short_circuit
    assert after_short_circuit["prompt_tokens"] == 0, after_short_circuit

    # 中断：合成的结果同样不算一次执行，被中断的轮也没有收尾。
    await session.send("go")
    await session.watch.expect("turn_started", within=15)
    response = await session.interrupt()
    assert response.get("ok") is True, response
    await session.watch.expect("interrupted", within=15)
    session.watch.assert_never("error")

    usage = await _usage(probe, session.session_id)
    assert usage["user_messages"] == 2, usage
    assert usage["turns"] == 1, usage
    assert usage["tool_calls"] == 0, usage
    assert usage["tool_time_ms"] == 0, usage
