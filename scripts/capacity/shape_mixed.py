#!/usr/bin/env python3
"""负载形态：混合满占空比（``mixed``）——**模型校验**用。

``--decode-duty``（默认 0.7）= 处于纯流式输出中的会话占比：前 ``round(duty × N)`` 个
会话跑纯流式（一轮覆盖整窗），其余跑工具循环（轮次装配复用 ``shape_tool_cycle``）。
两个会话以上时**两组都保证非空**——否则「混合」会退化成纯流式或纯工具，而基线档的
``turn.p99`` 还会变成 ``null``，让轮次判据整条阶梯失效。

于是「混合档的成本 ≈ 两种纯形态读数的线性叠加」是一个可检验的预测；偏差大就说明某个
成本项不与会话数线性（cache 抖动、GIL、页回收），必须人工解释而不是当噪声。

选**会话分组**而不是时间片：时间片下每个会话都在两种成本之间来回切，叠加预测不再成立，
本形态就失去了唯一的用途。
"""

from __future__ import annotations

import math

from shape_tool_cycle import TOOLS, cycle_turns
from shapes import CyclicScript, ShapeArgs, ShapePlan, covering_turn
from wing_probe.provider.script import Script

NAME = "mixed"
MODEL = f"capacity/{NAME}"
KNOBS: frozenset[str] = frozenset({"cycle_s", "decode_duty"})

STREAM_MODEL = f"{MODEL}/stream"
CYCLE_MODEL = f"{MODEL}/cycle"

#: 流式会话的剧本份额（每会话 1 轮 + 1 轮余量，同 ``pure_stream``）。
STREAM_TURNS_PER_SESSION = 2

#: 流式组在窗口末之后还多流的秒数（保证轮次边界落在采样窗口之外）。
TURN_SLACK_SECONDS = 1.0


def streaming_sessions(sessions: int, duty: float) -> int:
    """流式组的会话数。

    ``duty`` 的两个端点字面有效（1 = 全流式、0 = 全工具）；中间值取
    ``round(duty × N)``（四舍五入，不是银行家舍入）并**夹到两组都非空**——否则「混合」
    会退化成纯流式或纯工具，而基线档的 ``turn.p99`` 还会是 ``null``。
    单个会话无法拆分：进工具组，让基线档产出完成的轮次（轮次判据要一个可比的基线）。
    """
    if duty >= 1.0:
        return sessions
    if duty <= 0.0 or sessions <= 1:
        return 0
    rounded = int(math.floor(duty * sessions + 0.5))
    return min(max(rounded, 1), sessions - 1)


def plan(args: ShapeArgs) -> ShapePlan:
    """按 ``--decode-duty`` 切两组会话，各挂各的 model。"""
    streaming = streaming_sessions(args.sessions, args.decode_duty)
    cycling = args.sessions - streaming
    models: list[tuple[str, Script]] = []
    session_models: list[str] = []
    if streaming:
        turn = covering_turn(args, args.span_s + TURN_SLACK_SECONDS)
        models.append(
            (STREAM_MODEL, Script(*([turn] * (streaming * STREAM_TURNS_PER_SESSION))))
        )
        session_models.extend([STREAM_MODEL] * streaming)
    tool = "read"
    call_turn, reply_turn = cycle_turns(args, tool)
    for index in range(cycling):
        model_id = f"{CYCLE_MODEL}/s{index}"
        models.append((model_id, CyclicScript(call_turn, reply_turn)))
        session_models.append(model_id)
    variant = TOOLS[tool]
    return ShapePlan(
        models=tuple(models),
        session_models=tuple(session_models),
        tools=(str(variant["tool"]),),
        workspace_files=dict(variant["workspace_files"]),
    )
