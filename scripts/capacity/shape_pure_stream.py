#!/usr/bin/env python3
"""负载形态：纯长流式（``pure_stream``）——探并发边界的主负载。

每会话**一条覆盖整个档位时长的持续流**：一轮的 token 数 = ``rate × (warmup + dwell)``
（再加 1 秒余量保证它活过窗口末），帧大小 1 token、帧间隔 ``1/rate`` 秒。于是采样窗口
里每个会话都在同一轮之内——窗口内**没有轮次边界**，``gateway.cpu_cores`` 因此只含
「每事件成本」，不掺每轮的固定成本（上下文装配 / 落盘 / 审计 INSERT / 轮次事件）。
按会话数扫一遍就得到每事件成本这个常数；每轮固定成本要看别的形态。

刻意不含工具调用、不含历史体量、不含并发突刺——那些是别的形态要隔离的变量。

形态契约（``shapes.resolve`` 校验，新增形态照此实现）：

- ``NAME``：``--scenario`` 的取值；
- ``MODEL``：注册剧本用的 model id（``providers[].models`` 里的 id）；
- ``script(*, sessions, rate, span_s) -> Script``：够整个阶梯档位用的一条剧本；
- ``prompt_text(session_id, seq) -> str``：**全局唯一**的 prompt 文本（配对键）。
"""

from __future__ import annotations

import math

from wing_probe.provider.script import Script, Turn

NAME = "pure_stream"
MODEL = f"capacity/{NAME}"

#: 单轮在窗口末之后还多流的秒数：保证轮次边界落在采样窗口之外（含排程抖动）。
TURN_SLACK_SECONDS = 1.0

#: 每会话的剧本份额（轮数）。单轮形态下每会话只用掉 1 轮，留一倍余量给「轮次提前
#: 结束再发一轮」与网关重试——份额不够会让 Provider 5xx（剧本耗尽），那是假错误。
SCRIPT_TURNS_PER_SESSION = 2


def turn_tokens(rate: float, span_s: float) -> int:
    """一轮的 token 数（= 帧数，帧大小 1 token）：覆盖整个档位时长 + 余量。"""
    if rate <= 0:
        raise ValueError(f"turn_tokens() needs rate > 0, got {rate}")
    return max(1, math.ceil(rate * (span_s + TURN_SLACK_SECONDS)))


def prompt_text(session_id: str, seq: int) -> str:
    """第 ``seq`` 轮的用户正文（全局唯一：配对靠它在 ``RequestLog`` 里定位请求）。"""
    return f"load:{session_id}:{seq}"


def script(
    *,
    sessions: int,
    rate: float,
    span_s: float,
    frame_delay_s: float | None = None,
) -> Script:
    """整档共用的剧本：``sessions × SCRIPT_TURNS_PER_SESSION`` 个同款 Turn。

    ``frame_delay_s`` 是协调者**实测后**给的帧间隔（= ``1/rate`` 减去本机
    ``asyncio.sleep`` 的系统性超时，见 ``orchestrator.measure_pacing``）：Provider 是
    「睡一觉再写一帧」，不补偿的话注入速率恒低于 ``--rate``（本机超时约 2.1 ms/帧，
    50 tok/s 只能跑到约 45.6）。``None`` = 直接用 ``1/rate``（纯参数化，测试用）。

    剧本按 model 注册、由所有会话共享，每次请求消费一个 Turn。同一个 Turn 对象被
    重复引用（Provider 只读它），正文在内存里只有一份；但**每个在飞请求**都会在
    Provider 侧物化自己那份帧序列（``sse.stream_frames``），大档位的内存因此与
    ``sessions × rate × span`` 同阶——见 README「已知边界」。
    """
    if sessions < 1:
        raise ValueError(f"script() needs sessions >= 1, got {sessions}")
    delay = 1.0 / rate if frame_delay_s is None else frame_delay_s
    turn = Turn.of(text="x" * turn_tokens(rate, span_s), chunk=1, delay=max(0.0, delay))
    return Script(*([turn] * (sessions * SCRIPT_TURNS_PER_SESSION)))
