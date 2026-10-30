#!/usr/bin/env python3
"""负载形态：纯长流式（``pure_stream``）——探并发边界的主负载。

每会话**一条覆盖整个档位时长的持续流**：一轮的事件数 = ``rate × (warmup + dwell)``
（再加 1 秒余量保证它活过窗口末），每个事件带 ``--chunk`` 个字符、帧间隔 ``1/rate`` 秒。
于是采样窗口里每个会话都在同一轮之内——窗口内**没有轮次边界**，``gateway.cpu_cores``
因此只含「每事件成本」，不掺每轮的固定成本（上下文装配 / 落盘 / 审计 INSERT / 轮次事件）。
按会话数扫一遍就得到每事件成本这个常数；每轮固定成本要看别的形态。

刻意不含工具调用、不含历史体量、不含并发突刺——那些是别的形态要隔离的变量。
所有会话的轮完全相同，因此**共用一条 model 与一个剧本游标**（谁先请求都拿到同一份轮）。
"""

from __future__ import annotations

from shapes import ShapeArgs, ShapePlan, covering_turn
from wing_probe.provider.script import Script

NAME = "pure_stream"
MODEL = f"capacity/{NAME}"
KNOBS: frozenset[str] = frozenset()
"""不消费任何可选旋钮（``--chunk`` 对所有形态生效，不算形态旋钮）。"""

#: 单轮在窗口末之后还多流的秒数：保证轮次边界落在采样窗口之外（含排程抖动）。
TURN_SLACK_SECONDS = 1.0

#: 每会话的剧本份额（轮数）。单轮形态下每会话只用掉 1 轮，留一倍余量给「轮次提前
#: 结束再发一轮」与网关重试——份额不够会让 Provider 5xx（剧本耗尽），那是假错误。
SCRIPT_TURNS_PER_SESSION = 2


def plan(args: ShapeArgs) -> ShapePlan:
    """整档共用的剧本：``sessions × SCRIPT_TURNS_PER_SESSION`` 个同款 Turn。

    同一个 Turn 对象被重复引用（Provider 只读它），正文在内存里只有一份；但**每个
    在飞请求**都会在 Provider 侧物化自己那份帧序列（``sse.stream_frames``），大档位的
    内存因此与 ``sessions × rate × span`` 同阶（每帧字节数再乘 ``chunk``）。
    """
    turn = covering_turn(args, args.span_s + TURN_SLACK_SECONDS)
    script = Script(*([turn] * (args.sessions * SCRIPT_TURNS_PER_SESSION)))
    return ShapePlan(
        models=((MODEL, script),),
        session_models=tuple([MODEL] * args.sessions),
        tools=("Read",),
        drives_upstream=True,
        turn_boundaries=False,
    )
