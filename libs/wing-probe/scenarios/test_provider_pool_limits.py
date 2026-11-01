"""provider 连接池上限（``providers[].max_connections``）的可观测效果。

覆盖的断言点：

- ``test_connection_limit_serializes_concurrent_turns``：``max_connections: 1`` 时两个会话
  并发各跑一个「首帧被压住 ``HOLD`` 秒」的流式轮次 → 两轮**串行**；
- ``test_unlimited_pool_runs_concurrent_turns_in_parallel``：不配置（沿用 httpx 默认池）
  → 两轮**并行**。

证据面都是公开可观测事实，两层：

1. **事件时间线**：``turn_result`` 的到达时刻差。``Event.at`` 是"相对 env 启动的单调秒数"，
   两条会话共用同一个原点，因此可以直接相减（见 ``wing_probe/watch/timeline.py``）；
2. **假 Provider 请求留档**：两个上游请求的到达时刻差。这一层定位"被排队的是网关侧的连接池"
   ——只有一个连接名额时，第二个请求根本还没发出去（``LoggedRequest.at`` 是
   ``time.monotonic()`` 尺度，减去 ``env.started_at`` 与时间线同基准）。

阈值与理由：``HOLD = 2.0s`` 是单轮的流内停顿；串行时第二轮的 ``turn_result`` 要等到
第一轮整个流结束（≈ ``HOLD``）之后才开始自己的 ``HOLD``，因此两轮间隔的期望值就是
``HOLD``；并行时期望值 ≈ 0（实测 ~1e-4 s）。判定线取 ``HOLD * SERIAL_GAP_RATIO = 1.2s``
与 ``HOLD * PARALLEL_GAP_RATIO = 0.8s``：比例式让 ``HOLD`` 保持唯一旋钮（改停顿不用重算
两个数字），死区（0.8~1.2s）留给并行跑（``make test-probe -n``）时的进程 / 事件循环调度
抖动，同时不把"差一点点串行"判成通过。
"""

from __future__ import annotations

import asyncio

import pytest

from wing_probe import Probe, Turn

#: 场景私有 model 名（剧本按 model 名路由，场景之间不共享）。
SERIAL_MODEL = "probe/pool-limit-serial"
PARALLEL_MODEL = "probe/pool-limit-parallel"

#: 单轮流内首帧停顿（秒）——撑开"连接被占住"的窗口。
HOLD = 2.0

#: 串行下限 / 并行上限占 ``HOLD`` 的比例（理由见模块 docstring）。
SERIAL_GAP_RATIO = 0.6
PARALLEL_GAP_RATIO = 0.4

SERIAL_MIN_GAP = HOLD * SERIAL_GAP_RATIO
PARALLEL_MAX_GAP = HOLD * PARALLEL_GAP_RATIO


def _register_two_held_turns(probe: Probe, model: str) -> None:
    """同一 model 的两轮剧本，两轮都在首个内容帧前停顿 ``HOLD``。"""
    probe.register(
        model,
        Turn.of(text="held", first_delay=HOLD),
        Turn.of(text="held", first_delay=HOLD),
    )


async def _run_two_turns(probe: Probe, model: str) -> tuple[list[float], list[float]]:
    """两个会话并发各跑一轮；返回（两轮 ``turn_result`` 时刻, 两个上游请求到达时刻）。

    两组时刻同基准（相对 ``env.started_at``）：时间线本就是相对值，请求留档是绝对单调钟、
    减去同一原点后与前者可同基准对读。
    """
    first = await probe.session(model=model)
    second = await probe.session(model=model)

    results = await asyncio.gather(first.chat("held one"), second.chat("held two"))

    for session, result in zip((first, second), results, strict=True):
        assert result.data["subtype"] == "success", result.data
        session.watch.assert_never("error")

    origin = probe.env.started_at
    turn_ats = sorted(result.at for result in results)
    request_ats = sorted(entry.at - origin for entry in probe.requests_for(model))
    return turn_ats, request_ats


@pytest.mark.probe_env(models=[SERIAL_MODEL], provider_extra={"max_connections": 1})
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_connection_limit_serializes_concurrent_turns(probe: Probe) -> None:
    """``max_connections: 1`` 时两个并发轮次必须串行。

    WHEN provider 的连接池只给一个名额，两个会话各发一个流内停顿 ``HOLD`` 秒的轮次
    THEN 第二轮的 ``turn_result`` 至少落后第一轮 ``SERIAL_MIN_GAP``；且第二个上游请求的
    到达时刻同样后移——排队发生在网关的客户端连接池里，不是上游自己串行。
    """
    _register_two_held_turns(probe, SERIAL_MODEL)

    turn_ats, request_ats = await _run_two_turns(probe, SERIAL_MODEL)

    assert len(request_ats) == 2, request_ats
    assert turn_ats[1] - turn_ats[0] >= SERIAL_MIN_GAP, turn_ats
    assert request_ats[1] - request_ats[0] >= SERIAL_MIN_GAP, request_ats


@pytest.mark.probe_env(models=[PARALLEL_MODEL])
@pytest.mark.timeout(120)
@pytest.mark.asyncio
async def test_unlimited_pool_runs_concurrent_turns_in_parallel(probe: Probe) -> None:
    """不配置连接上限时（httpx 默认池）两个轮次并行。

    对照组：同一份剧本、同样的并发方式，只差 ``max_connections``——两轮在
    ``PARALLEL_MAX_GAP`` 内先后完成，两个上游请求也几乎同时到达。
    """
    _register_two_held_turns(probe, PARALLEL_MODEL)

    turn_ats, request_ats = await _run_two_turns(probe, PARALLEL_MODEL)

    assert len(request_ats) == 2, request_ats
    assert turn_ats[1] - turn_ats[0] <= PARALLEL_MAX_GAP, turn_ats
    assert request_ats[1] - request_ats[0] <= PARALLEL_MAX_GAP, request_ats
