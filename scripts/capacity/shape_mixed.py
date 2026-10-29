#!/usr/bin/env python3
"""负载形态：混合满占空比（``mixed``）——**模型校验**用。

``--decode-duty``（默认 0.7）= 处于纯流式输出中的会话占比：前 ``round(duty × N)`` 个
会话跑 ``pure_stream``（一轮覆盖整窗），其余跑 ``tool_cycle``。于是「混合档的成本 ≈
两种纯形态读数的线性叠加」是一个可检验的预测——偏差超过 30% 就说明某个成本项不与
会话数线性（cache 抖动、GIL、页回收），必须在文档里说清为什么。

选**会话分组**而不是时间片：时间片下每个会话都在两种成本之间来回切，叠加预测不再成立，
本形态就失去了唯一的用途。
"""

from __future__ import annotations

from shapes import CyclicScript, ShapeArgs, ShapePlan, covering_turn, prompt_key
from shape_tool_cycle import REPLY_CHARS, TOOLS
from wing_probe.provider.script import Script, ToolCall, Turn

NAME = "mixed"
MODEL = f"capacity/{NAME}"
KNOBS: frozenset[str] = frozenset({"cycle_s", "decode_duty"})

STREAM_MODEL = f"{MODEL}/stream"
CYCLE_MODEL = f"{MODEL}/cycle"

#: 流式会话的剧本份额（每会话 1 轮 + 1 轮余量，同 ``pure_stream``）。
STREAM_TURNS_PER_SESSION = 2

#: 流式组在窗口末之后还多流的秒数（保证轮次边界落在采样窗口之外）。
TURN_SLACK_SECONDS = 1.0


def prompt_text(session_id: str, seq: int) -> str:
    """第 ``seq`` 轮的用户正文（全局唯一：配对靠它在 ``RequestLog`` 里定位请求）。"""
    return prompt_key(session_id, seq)


def plan(args: ShapeArgs) -> ShapePlan:
    """按 ``--decode-duty`` 切两组会话，各挂各的 model。"""
    streaming = min(max(int(round(args.decode_duty * args.sessions)), 0), args.sessions)
    cycling = args.sessions - streaming
    models: list[tuple[str, Script]] = []
    session_models: list[str] = []
    if streaming:
        turn = covering_turn(args, args.span_s + TURN_SLACK_SECONDS)
        models.append(
            (STREAM_MODEL, Script(*([turn] * (streaming * STREAM_TURNS_PER_SESSION))))
        )
        session_models.extend([STREAM_MODEL] * streaming)
    variant = TOOLS["read"]
    first_delay = args.cycle_s if args.cycle_s > 0 else None
    call_turn = Turn.of(
        tool_calls=[ToolCall(variant["tool"], dict(variant["arguments"]))],
        first_delay=first_delay,
    )
    reply_turn = Turn.of(text="." * REPLY_CHARS * args.chunk, chunk=args.chunk)
    for index in range(cycling):
        model_id = f"{CYCLE_MODEL}/s{index}"
        models.append((model_id, CyclicScript(call_turn, reply_turn)))
        session_models.append(model_id)
    return ShapePlan(
        models=tuple(models),
        session_models=tuple(session_models),
        tools=(str(variant["tool"]),),
        workspace_files=dict(variant["workspace_files"]),
    )
